//! Standalone managed repository bases. No application or control-plane identity.
//! Published generations are immutable; refreshing never touches named workspaces.
use super::storage::{self, Backend, Btrfs};
use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const REPOSITORY_FORMAT: &str = "ai-fence-repository-v1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub format: String,
    pub id: String,
    pub remote: String,
    pub branch: String,
    pub directory: Identity,
    pub filesystem_id: [u8; 16],
    pub master: Identity,
    pub current: Generation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub id: String,
    pub commit: String,
    pub created_at: String,
    pub snapshot: Identity,
}

fn checked_id(id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).context("invalid repository/generation identity")?;
    Ok(())
}

/// Prefer a private store near the project on its actual filesystem. This is
/// deliberately limited to owned writable ancestors and mounted Btrfs roots;
/// discovering storage never grants permission to snapshot another directory.
pub fn automatic_directory(near: &Path, preferred: &Path) -> Result<PathBuf> {
    use crate::workspace_snapshot::discovery::filesystem_id;
    let near = near.canonicalize()?;
    let mut candidates = Vec::new();
    if let Some(parent) = preferred.parent().filter(|p| p.is_dir()) {
        candidates.push(parent.canonicalize()?.join("workspaces"));
    }
    for parent in near.ancestors().skip(1) {
        if storage::owned_directory(parent).is_ok() && filesystem_id(parent).is_ok() {
            candidates.push(parent.join(".ai-fence-workspaces"));
            break;
        }
    }
    #[cfg(target_os = "linux")]
    for mount in mount_roots(&fs::read_to_string("/proc/self/mountinfo")?)? {
        if storage::owned_directory(&mount).is_ok() {
            candidates.push(mount.join(".ai-fence-workspaces"));
        }
    }
    let target_fs = filesystem_id(&near).ok();
    let mut eligible = std::collections::BTreeMap::new();
    for candidate in candidates {
        let parent = candidate.parent().context("store needs parent")?;
        if storage::owned_directory(parent).is_err() || !writable(parent) {
            continue;
        }
        let Ok(identity) = filesystem_id(parent) else {
            continue;
        };
        if target_fs.as_ref().is_some_and(|id| id != &identity) {
            continue;
        }
        if candidate == preferred && target_fs.as_ref() == Some(&identity) {
            return Ok(candidate);
        }
        eligible.entry(identity).or_insert(candidate);
    }
    anyhow::ensure!(eligible.len() == 1,
        "no unambiguous private writable Btrfs store; select workspace init --directory PATH (no mount, elevation or fallback attempted)");
    Ok(eligible.into_values().next().unwrap())
}

fn writable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: a live, nul-terminated path, no retained pointers.
        unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

#[cfg(target_os = "linux")]
fn mount_roots(text: &str) -> Result<Vec<PathBuf>> {
    let selected = text
        .lines()
        .filter(|line| {
            line.split_once(" - ")
                .is_some_and(|(_, fs)| fs.starts_with("btrfs "))
        })
        .collect::<Vec<_>>()
        .join("\n");
    storage::nested_mounts_from(&selected, Path::new("/"))
}

fn subvolume(arguments: &[&str], paths: &[&Path]) -> Result<()> {
    let result = Command::new("btrfs")
        .arg("subvolume")
        .args(arguments)
        .args(paths)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    anyhow::ensure!(
        result.success(),
        "managed repository Btrfs operation failed; check store permissions and capacity"
    );
    Ok(())
}

/// Git receives no shell text. Hooks, prompts and recursive submodules are
/// disabled. A finite process-group deadline contains its own SSH children.
fn git(root: &Path, args: &[&str], environment: &[(OsString, OsString)]) -> Result<()> {
    let mut command = Command::new("git");
    clear_git_environment(&mut command);
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        parent_death_signal(&mut command);
    }
    let mut child = command.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(
                status.success(),
                "managed repository Git operation failed; last published generation is preserved"
            );
            return Ok(());
        }
        if start.elapsed() >= Duration::from_secs(120) {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
            }
            let _ = child.wait();
            bail!("managed repository Git operation timed out; no generation published");
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

pub(super) fn clear_git_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy();
        if key.starts_with("GIT_")
            && !matches!(
                key.as_ref(),
                "GIT_SSH"
                    | "GIT_SSH_COMMAND"
                    | "GIT_SSH_VARIANT"
                    | "GIT_ASKPASS"
                    | "GIT_SSL_CAINFO"
                    | "GIT_SSL_CAPATH"
            )
        {
            command.env_remove(name);
        }
    }
}

#[cfg(unix)]
pub(super) fn parent_death_signal(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id();
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent as i32 {
                    return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

impl Registry {
    /// Human-facing entry point: infer the current project's remote/default
    /// branch and select Btrfs storage without a settings file or manual UUID.
    pub fn repository_open(
        &self,
        remote: Option<&str>,
        branch: Option<&str>,
        directory: Option<&Path>,
    ) -> Result<Repository> {
        let cwd = std::env::current_dir()?;
        let remote = match remote {
            Some(value) => value.to_owned(),
            None => {
                let output = source::output(&cwd, &["remote"], 16384)?;
                let names = std::str::from_utf8(&output)?.lines().collect::<Vec<_>>();
                let selected = if names.contains(&"origin") {
                    "origin"
                } else {
                    anyhow::ensure!(names.len() == 1, "select a Git remote explicitly");
                    names[0]
                };
                String::from_utf8(source::output(
                    &cwd,
                    &["remote", "get-url", selected],
                    16384,
                )?)?
                .trim()
                .to_owned()
            }
        };
        let repositories = self.root.join("repositories");
        if repositories.exists() {
            storage::owned_directory(&repositories)?;
            for entry in fs::read_dir(repositories)? {
                let path = entry?.path();
                let record: Repository = storage::read_json(&path)?;
                if record.remote == remote && branch.is_none_or(|branch| branch == record.branch) {
                    let record = self.repository(&record.id)?;
                    if let Some(directory) = directory {
                        anyhow::ensure!(
                            directory.canonicalize()?
                                == record
                                    .directory
                                    .path
                                    .parent()
                                    .context("missing source store")?,
                            "repository storage already registered"
                        );
                    }
                    return Ok(record);
                }
            }
        }
        let branch = match branch {
            Some(branch) => branch.to_owned(),
            None => {
                let output = source::output(
                    &cwd,
                    &["ls-remote", "--symref", "--", &remote, "HEAD"],
                    16384,
                )?;
                std::str::from_utf8(&output)?
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("ref: refs/heads/")
                            .and_then(|value| value.strip_suffix("\tHEAD"))
                    })
                    .context("remote default branch unavailable; select --branch explicitly")?
                    .to_owned()
            }
        };
        self.repository_ensure(
            &uuid::Uuid::new_v4().to_string(),
            &remote,
            &branch,
            directory,
            &[],
        )
    }

    fn repository_path(&self, id: &str) -> Result<PathBuf> {
        checked_id(id)?;
        Ok(self.root.join("repositories").join(format!("{id}.json")))
    }

    pub fn repository(&self, id: &str) -> Result<Repository> {
        let record: Repository = storage::read_json(&self.repository_path(id)?)?;
        anyhow::ensure!(
            record.format == REPOSITORY_FORMAT && record.id == id,
            "unsupported repository identity"
        );
        record.directory.validate()?;
        record.master.validate()?;
        record.current.snapshot.validate()?;
        anyhow::ensure!(
            record.directory.matches(&record.directory.path)
                && record.master.matches(&record.master.path)
                && crate::workspace_snapshot::discovery::filesystem_id(&record.directory.path)?
                    == record.filesystem_id,
            "repository storage missing/replaced; reconnect it (no fallback created)"
        );
        self.verify_generation(&record.current)?;
        Ok(record)
    }

    pub(super) fn verify_generation(&self, generation: &Generation) -> Result<()> {
        checked_id(&generation.id)?;
        anyhow::ensure!(
            generation.snapshot.matches(&generation.snapshot.path)
                && Btrfs.read_only(&generation.snapshot.path)?
                && storage::head(&generation.snapshot.path).as_ref() == Some(&generation.commit),
            "published repository generation missing, replaced or writable"
        );
        Ok(())
    }

    pub fn repository_generation(&self, id: &str, generation: Option<&str>) -> Result<Generation> {
        let record = self.repository(id)?;
        let Some(generation) = generation else {
            return Ok(record.current);
        };
        checked_id(generation)?;
        let root = record.directory.path.join("bases");
        let value: Generation = storage::read_json(&root.join(format!("{generation}.json")))?;
        anyhow::ensure!(
            value.id == generation && value.snapshot.path == root.join(generation),
            "generation belongs to another repository"
        );
        self.verify_generation(&value)?;
        Ok(value)
    }

    /// Registration is idempotent only for the identical remote/branch/store.
    /// Caller-supplied namespaces belong in a private registry, never in Git URLs.
    pub fn repository_ensure(
        &self,
        id: &str,
        remote: &str,
        branch: &str,
        directory: Option<&Path>,
        environment: &[(OsString, OsString)],
    ) -> Result<Repository> {
        checked_id(id)?;
        anyhow::ensure!(
            !remote.is_empty() && !remote.starts_with('-') && !remote.chars().any(char::is_control),
            "invalid Git remote"
        );
        if let Ok(url) = reqwest::Url::parse(remote) {
            anyhow::ensure!(
                url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && (!matches!(url.scheme(), "http" | "https") || url.username().is_empty()),
                "use an external Git credential helper; remote URLs must not contain credentials"
            );
        }
        anyhow::ensure!(
            !branch.is_empty() && !branch.starts_with('-') && !branch.chars().any(char::is_control),
            "invalid branch"
        );
        let _lock = storage::lock(self)?;
        if self.repository_path(id)?.exists() {
            let record = self.repository(id)?;
            anyhow::ensure!(
                record.remote == remote && record.branch == branch,
                "repository identity already belongs to another remote/branch"
            );
            if let Some(directory) = directory {
                anyhow::ensure!(
                    directory.canonicalize()?
                        == record
                            .directory
                            .path
                            .parent()
                            .context("repository store parent missing")?,
                    "repository store cannot change implicitly"
                );
            }
            return Ok(record);
        }
        let directory = match directory {
            Some(path) => path.to_path_buf(),
            None => {
                let configured = self.root.join("settings.json");
                if configured.exists() {
                    self.directory()?
                } else {
                    automatic_directory(&std::env::current_dir()?, &self.directory()?)?
                }
            }
        };
        storage::private_directory(&directory)?;
        let directory = directory.canonicalize()?;
        let filesystem_id = crate::workspace_snapshot::discovery::filesystem_id(&directory)?;
        anyhow::ensure!(
            fs2::available_space(&directory)? >= 1073741824,
            "repository store below its 1 GiB free-space floor"
        );
        let root = directory.join(id);
        anyhow::ensure!(
            !root.exists(),
            "unregistered repository directory exists; explicit recovery required"
        );
        storage::private_directory(&root)?;
        storage::private_directory(&root.join("bases"))?;
        let master = root.join("master");
        subvolume(&["create"], &[&master])?;
        let master_identity = Identity::read(&master)?;
        let initialized = git(
            &root,
            &[
                "clone",
                "--no-local",
                "--no-hardlinks",
                "--single-branch",
                "--branch",
                branch,
                "--",
                remote,
                "master",
            ],
            environment,
        );
        if let Err(error) = initialized {
            // This is a fresh engine-owned checkout, never a user's workspace.
            // A normal failed clone can be cleaned immediately. Crash leftovers
            // still refuse implicit adoption and require explicit inspection.
            if master_identity.matches(&master) && Btrfs.validate(&master, &directory).is_ok() {
                Btrfs.delete(&master)?;
                fs::remove_dir(root.join("bases"))?;
                fs::remove_dir(&root)?;
            }
            return Err(error);
        }
        storage::independent_git(&master)?;
        let current = self.publish_generation(&root, &master)?;
        let record = Repository {
            format: REPOSITORY_FORMAT.into(),
            id: id.into(),
            remote: remote.into(),
            branch: branch.into(),
            directory: Identity::read(&root)?,
            filesystem_id,
            master: Identity::read(&master)?,
            current,
        };
        storage::save_json(&self.repository_path(id)?, &record)?;
        Ok(record)
    }

    fn publish_generation(&self, root: &Path, master: &Path) -> Result<Generation> {
        storage::independent_git(master)?;
        let id = uuid::Uuid::new_v4().to_string();
        let path = root.join("bases").join(&id);
        subvolume(&["snapshot", "-r"], &[master, &path])?;
        let generation = Generation {
            id,
            commit: storage::head(&path).context("published base has no commit")?,
            created_at: chrono::Utc::now().to_rfc3339(),
            snapshot: Identity::read(&path)?,
        };
        self.verify_generation(&generation)?;
        storage::save_json(
            &root.join("bases").join(format!("{}.json", generation.id)),
            &generation,
        )?;
        Ok(generation)
    }

    pub fn repository_refresh(
        &self,
        id: &str,
        environment: &[(OsString, OsString)],
    ) -> Result<Repository> {
        self.repository_prune(id, 3, 900, true)?;
        let _lock = storage::lock(self)?;
        let mut record = self.repository(id)?;
        anyhow::ensure!(
            fs2::available_space(&record.directory.path)? >= 1073741824,
            "source store below its free-space floor after eligible base cleanup"
        );
        let master = &record.master.path;
        storage::independent_git(master)?;
        let branch = storage::git(master, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        anyhow::ensure!(
            branch.status.success() && std::str::from_utf8(&branch.stdout)?.trim() == record.branch,
            "managed master branch changed; explicit repair required"
        );
        let status = storage::git(
            master,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )?;
        anyhow::ensure!(
            status.status.success() && status.stdout.is_empty(),
            "managed master is dirty; preserve and repair explicitly"
        );
        let refspec = format!("refs/heads/{0}:refs/remotes/origin/{0}", record.branch);
        git(
            master,
            &[
                "fetch",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
                &record.remote,
                &refspec,
            ],
            environment,
        )?;
        git(
            master,
            &["merge", "--ff-only", "--no-edit", "FETCH_HEAD"],
            environment,
        )?;
        if storage::head(master).as_ref() != Some(&record.current.commit) {
            record.current = self.publish_generation(&record.directory.path, master)?;
            storage::save_json(&self.repository_path(id)?, &record)?;
        }
        Ok(record)
    }

    pub fn repository_source(
        &self,
        id: &str,
        generation: Option<&str>,
        query: source::Query,
        consumer: &str,
        lease_id: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<serde_json::Value> {
        let generation = self.repository_generation(id, generation)?;
        self.repository_pin(id, &generation.id, lease_id, consumer, expires_at, true)?;
        let mut pins = vec![lease_id.to_owned()];
        let value = (|| {
            match query {
                source::Query::Diff {
                    base_generation_id,
                    prefix,
                } => {
                    let base = self.repository_generation(id, Some(&base_generation_id))?;
                    // One consumer can hold multiple generations independently.
                    let base_lease = uuid::Uuid::new_v4().to_string();
                    self.repository_pin(id, &base.id, &base_lease, consumer, expires_at, true)?;
                    pins.push(base_lease);
                    source::diff(&generation, &base, &prefix)
                }
                query => source::read(&generation, query),
            }
        })();
        for pin in &pins {
            self.repository_finish_read(id, pin, consumer)?;
        }
        let mut value = value?;
        value["view_lease_ids"] = serde_json::json!(pins);
        value["view_expires_at"] = serde_json::json!(expires_at);
        Ok(value)
    }

    pub fn repository_workspace(&self, id: &str, name: &str) -> Result<Record> {
        let repository = self.repository(id)?;
        // Master identity remains stable across fast-forwards; its immutable
        // generations may change without changing workspace names or histories.
        self.create_source_with(
            name,
            storage::Source {
                path: repository.current.snapshot.path,
                project: repository.master.clone(),
                logical_root: repository.master.path,
                checkpoint: None,
            },
            Some(&repository.directory.path.join("workspaces")),
            &Btrfs,
        )
    }

    pub fn repository_workspace_latest(&self, id: &str, name: &str) -> Result<Record> {
        let repository = self.repository(id)?;
        if self.lookup(name, Some(&repository.master.path)).is_ok() {
            return self.repository_workspace(id, name);
        }
        self.repository_refresh(id, &[])?;
        self.repository_workspace(id, name)
    }
}
