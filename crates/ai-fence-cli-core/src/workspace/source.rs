//! Bounded, committed-source projections. Git objects avoid following working
//! tree symlinks and never expose repository configuration or arbitrary files.
use super::*;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    List {
        #[serde(default)]
        prefix: String,
        #[serde(default)]
        after: String,
    },
    Read {
        path: String,
        #[serde(default = "one")]
        start_line: usize,
    },
    Search {
        literal: String,
        #[serde(default)]
        prefix: String,
    },
    Diff {
        base_generation_id: String,
        #[serde(default)]
        prefix: String,
    },
}
fn one() -> usize {
    1
}

fn permitted(path: &str, prefix: bool) -> Result<()> {
    anyhow::ensure!(
        path.len() <= 2048 && !path.chars().any(char::is_control),
        "invalid source path"
    );
    if path.is_empty() && prefix {
        return Ok(());
    }
    anyhow::ensure!(
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|p| matches!(p, std::path::Component::Normal(_))),
        "source path must be relative without traversal"
    );
    for part in path.split('/') {
        let lower = part.to_ascii_lowercase();
        anyhow::ensure!(
            !(matches!(
                lower.as_str(),
                ".git"
                    | ".ssh"
                    | ".aws"
                    | ".gnupg"
                    | "credentials"
                    | "credentials.json"
                    | "id_rsa"
                    | "id_ed25519"
            ) || lower == ".env"
                || lower.starts_with(".env.")
                || lower.ends_with(".pem")
                || lower.ends_with(".key")),
            "sensitive source path excluded"
        );
    }
    Ok(())
}

pub(super) fn output(root: &Path, args: &[&str], maximum: usize) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    repository::clear_git_environment(&mut command);
    command
        .args([
            "--no-replace-objects",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        repository::parent_death_signal(&mut command);
    }
    let mut child = command.spawn()?;
    let pipe = child.stdout.take().context("source output unavailable")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = pipe
            .take(maximum as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let start = Instant::now();
    let result = loop {
        match receiver.recv_timeout(Duration::from_millis(40)) {
            Ok(bytes) => break bytes.map_err(anyhow::Error::from),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break Err(anyhow::anyhow!("source reader stopped"))
            }
            Err(_) if start.elapsed() < Duration::from_secs(15) => continue,
            Err(_) => break Err(anyhow::anyhow!("source read timed out")),
        }
    };
    if result.as_ref().is_err() || result.as_ref().is_ok_and(|v| v.len() > maximum) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        #[cfg(not(unix))]
        {
            let _ = child.kill();
        }
    }
    // A bounded pipe may finish before the process exits. Git's fixed local
    // read commands have no helper children, but still keep the deadline.
    while child.try_wait()?.is_none() {
        if start.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = child.wait()?;
    let _ = reader.join();
    let bytes = result?;
    anyhow::ensure!(
        status.success() && bytes.len() <= maximum,
        "source object unavailable or exceeds read bound"
    );
    Ok(bytes)
}

fn files(root: &Path) -> Result<Vec<String>> {
    let bytes = output(root, &["ls-tree", "-r", "-z", "HEAD"], 8 * 1024 * 1024)?;
    let mut files = Vec::new();
    for entry in bytes.split(|b| *b == 0).filter(|v| !v.is_empty()) {
        // Only ordinary blobs. Symlinks and gitlinks never become source files.
        if !entry.starts_with(b"100644 blob ") && !entry.starts_with(b"100755 blob ") {
            continue;
        }
        let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
            bail!("invalid source tree")
        };
        let path = std::str::from_utf8(&entry[tab + 1..])?;
        if permitted(path, false).is_ok() {
            files.push(path.to_owned());
        }
    }
    files.sort();
    Ok(files)
}

fn text(root: &Path, path: &str) -> Result<String> {
    permitted(path, false)?;
    let bytes = output(
        root,
        &["cat-file", "blob", &format!("HEAD:{path}")],
        256 * 1024,
    )?;
    anyhow::ensure!(!bytes.contains(&0), "binary source omitted");
    String::from_utf8(bytes).context("source is not UTF-8")
}

/// The caller authenticates the chosen immutable generation before entering.
/// Returned metadata always identifies that exact commit, never a live checkout.
pub fn read(generation: &repository::Generation, query: Query) -> Result<serde_json::Value> {
    let root = &generation.snapshot.path;
    anyhow::ensure!(
        generation.snapshot.matches(root)
            && storage::head(root).as_ref() == Some(&generation.commit),
        "source generation changed"
    );
    let files = files(root)?;
    let data = match query {
        Query::Diff { .. } => bail!("generation diffs must use the repository source API"),
        Query::List { prefix, after } => {
            permitted(&prefix, true)?;
            anyhow::ensure!(after.len() <= 2048, "invalid source cursor");
            let selected = files
                .into_iter()
                .filter(|p| p.starts_with(&prefix) && p > &after)
                .take(201)
                .collect::<Vec<_>>();
            let more = selected.len() > 200;
            let selected = selected.into_iter().take(200).collect::<Vec<_>>();
            serde_json::json!({"files":selected,"next_after":if more { selected.last() } else { None }})
        }
        Query::Read { path, start_line } => {
            anyhow::ensure!(
                start_line > 0 && start_line <= 100000,
                "invalid source line"
            );
            anyhow::ensure!(
                files.binary_search(&path).is_ok(),
                "source file not in approved ordinary tree"
            );
            let content = text(root, &path)?;
            let selected = content
                .lines()
                .skip(start_line - 1)
                .take(201)
                .collect::<Vec<_>>();
            let mut lines = Vec::new();
            let mut bytes = 0;
            for line in selected.iter().take(200) {
                if bytes + line.len() > 24000 {
                    break;
                }
                bytes += line.len();
                lines.push(*line);
            }
            let next = (lines.len() < selected.len()).then_some(start_line + lines.len());
            serde_json::json!({"path":path,"start_line":start_line,"lines":lines,"next_line":next})
        }
        Query::Search { literal, prefix } => {
            permitted(&prefix, true)?;
            anyhow::ensure!(
                !literal.is_empty()
                    && literal.len() <= 200
                    && !literal.chars().any(char::is_control),
                "invalid literal source search"
            );
            let mut hits = Vec::new();
            let mut scanned = 0;
            let mut more = false;
            let mut skipped = 0;
            let started = Instant::now();
            for path in files.iter().filter(|p| p.starts_with(&prefix)) {
                if scanned >= 200 || hits.len() >= 50 || started.elapsed() > Duration::from_secs(10)
                {
                    more = true;
                    break;
                }
                scanned += 1;
                match text(root, path) {
                    Ok(content) => {
                        for (line, value) in content.lines().enumerate() {
                            if value.contains(&literal) {
                                hits.push(serde_json::json!({"path":path,"line":line+1,"text":value.chars().take(300).collect::<String>()}));
                                if hits.len() == 50 {
                                    more = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(_) => skipped += 1,
                }
            }
            serde_json::json!({"matches":hits,"files_scanned":scanned,"files_skipped":skipped,"truncated":more,"guidance":"Narrow prefix if truncated; binary and oversized files are skipped."})
        }
    };
    Ok(
        serde_json::json!({"generation_id":generation.id,"commit":generation.commit,"captured_at":generation.created_at,"source_kind":"committed_base","data":data}),
    )
}

pub(super) fn diff(
    generation: &repository::Generation,
    base: &repository::Generation,
    prefix: &str,
) -> Result<serde_json::Value> {
    permitted(prefix, true)?;
    let root = &generation.snapshot.path;
    let eligible = files(root)?
        .into_iter()
        .chain(files(&base.snapshot.path)?)
        .collect::<std::collections::HashSet<_>>();
    let bytes = output(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--name-only",
            "-z",
            &base.commit,
            &generation.commit,
            "--",
        ],
        8 * 1024 * 1024,
    )?;
    let paths = bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(std::str::from_utf8)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let selected = paths
        .into_iter()
        .filter(|p| p.starts_with(prefix) && eligible.contains(*p))
        .take(201)
        .collect::<Vec<_>>();
    Ok(
        serde_json::json!({"generation_id":generation.id,"commit":generation.commit,"captured_at":generation.created_at,
        "source_kind":"committed_base","data":{"base_generation_id":base.id,"base_commit":base.commit,"changed_files":selected.iter().take(200).collect::<Vec<_>>(),"truncated":selected.len()>200}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_paths_refuse_traversal_git_and_credentials() {
        for path in [
            "../secret",
            "/etc/passwd",
            ".git/config",
            "src/../../secret",
            ".env",
            "config/.env.prod",
            "deploy/private.key",
            ".ssh/id_ed25519",
            "file\nname",
        ] {
            assert!(permitted(path, false).is_err(), "{path}");
        }
        for path in ["src/lib.rs", "Cargo.toml", "docs/security.md"] {
            assert!(permitted(path, false).is_ok());
        }
    }
}
