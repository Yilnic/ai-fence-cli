use super::*;
use fs2::FileExt;
use serde::de::DeserializeOwned;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::{Command, Output};

pub(super) fn valid_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 64 && name.as_bytes()[0].is_ascii_alphanumeric()
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
        "workspace names must start with a letter/digit and contain at most 64 letters, digits, dots, underscores or hyphens"
    );
    Ok(())
}

fn owned(path: &Path, directory: bool) -> Result<()> {
    let info = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        !info.file_type().is_symlink() && info.is_dir() == directory,
        "workspace metadata must be an owned {}: {}",
        if directory {
            "directory"
        } else {
            "regular file"
        },
        path.display()
    );
    if !directory {
        anyhow::ensure!(info.is_file(), "workspace metadata is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or memory accesses.
        anyhow::ensure!(
            info.uid() == unsafe { libc::geteuid() } && info.mode() & 0o022 == 0,
            "workspace metadata is unowned or writable by another user: {}",
            path.display()
        );
    }
    Ok(())
}

pub(super) fn owned_directory(path: &Path) -> Result<()> {
    owned(path, true)
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(path)?;
    }
    owned_directory(path)
}

pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    owned(path, false)?;
    anyhow::ensure!(
        fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "workspace metadata exceeds size limit"
    );
    serde_json::from_reader(File::open(path)?)
        .with_context(|| format!("invalid workspace metadata: {}", path.display()))
}

pub(super) fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("workspace metadata needs a parent")?;
    private_directory(parent)?;
    if fs::symlink_metadata(path).is_ok() {
        owned(path, false)?;
    }
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub(super) fn lock(registry: &Registry) -> Result<Lock> {
    private_directory(&registry.root)?;
    let path = registry.root.join(".lock");
    if fs::symlink_metadata(&path).is_ok() {
        owned(&path, false)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    file.lock_exclusive()?;
    Ok(Lock(file))
}

pub(super) fn records(registry: &Registry) -> Result<Vec<Record>> {
    let directory = registry.root.join("records");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    owned_directory(&registry.root)?;
    owned_directory(&directory)?;
    let mut records = Vec::new();
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == "json") {
            let id = path
                .file_stem()
                .and_then(|v| v.to_str())
                .context("invalid workspace record filename")?;
            records.push(registry.read(id)?);
        }
    }
    records.sort_by(|a, b| {
        a.project
            .path
            .cmp(&b.project.path)
            .then(a.name.cmp(&b.name))
    });
    Ok(records)
}

pub(super) fn git(root: &Path, args: &[&str]) -> Result<Output> {
    Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "submodule.recurse=false",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("cannot run git for workspace inspection")
}

pub(super) fn head(root: &Path) -> Option<String> {
    git(root, &["rev-parse", "--verify", "HEAD"])
        .ok()
        .filter(|v| v.status.success())
        .map(|v| String::from_utf8_lossy(&v.stdout).trim().to_owned())
}

/// Reject layouts where a CoW clone could still write another checkout's Git state.
pub(super) fn independent_git(root: &Path) -> Result<()> {
    let mut queue = vec![root.to_path_buf()];
    while let Some(path) = queue.pop() {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if entry.file_name() == ".git" {
                anyhow::ensure!(
                    path == root,
                    "nested repositories/submodules require deliberate Git isolation"
                );
                anyhow::ensure!(
                    entry.file_type()?.is_dir(),
                    "external/shared .git metadata cannot be cloned; use an independent checkout"
                );
                continue;
            }
            if entry.file_type()?.is_dir() {
                queue.push(entry.path());
            }
        }
    }
    if !root.join(".git").exists() {
        return Ok(());
    }
    for option in ["--git-dir", "--git-common-dir", "--show-toplevel"] {
        let output = git(root, &["rev-parse", "--path-format=absolute", option])?;
        anyhow::ensure!(
            output.status.success(),
            "cannot verify Git metadata independence"
        );
        let path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        anyhow::ensure!(
            path.canonicalize()?.starts_with(root),
            "Git metadata points outside this workspace"
        );
    }
    let index = git(root, &["ls-files", "--stage", "-z"])?;
    anyhow::ensure!(
        index.status.success(),
        "cannot inspect Git submodule metadata"
    );
    anyhow::ensure!(
        !index
            .stdout
            .split(|byte| *byte == 0)
            .any(|entry| entry.starts_with(b"160000 ")),
        "Git submodules require deliberate isolation"
    );
    let mut queue = vec![root.join(".git")];
    while let Some(path) = queue.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            anyhow::ensure!(
                !entry.file_type()?.is_symlink(),
                "symlinked Git metadata cannot be isolated safely"
            );
            let entry_path = entry.path();
            let git_root = root.join(".git");
            let relative = entry_path.strip_prefix(&git_root)?;
            anyhow::ensure!(
                relative != Path::new("objects/info/alternates")
                    && relative != Path::new("worktrees"),
                "shared Git objects/worktree metadata require deliberate Git isolation"
            );
            anyhow::ensure!(!entry.file_name().to_string_lossy().ends_with(".lock"), "Git metadata has an active/incomplete write lock; finish or inspect it before creating a workspace");
            if entry.file_type()?.is_dir() {
                queue.push(entry.path());
            }
        }
    }
    Ok(())
}

pub(super) fn dirty(record: &Record) -> Result<bool> {
    let root = &record.workspace.path;
    anyhow::ensure!(
        record.workspace.matches(root),
        "workspace missing or replaced; explicit recovery required"
    );
    if !root.join(".git").exists() {
        return Ok(fs::read_dir(root)?
            .any(|entry| entry.is_err() || entry.is_ok_and(|e| e.file_name() != MARKER)));
    }
    independent_git(root)?;
    if head(root) != record.base_head {
        return Ok(true);
    }
    let output = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    anyhow::ensure!(
        output.status.success(),
        "Git status failed; workspace is protected from removal"
    );
    Ok(output
        .stdout
        .split(|&byte| byte == 0)
        .any(|item| !item.is_empty() && item.get(3..) != Some(MARKER.as_bytes())))
}

pub(super) trait Backend {
    fn validate(&self, source: &Path, directory: &Path) -> Result<()>;
    fn snapshot(&self, source: &Path, destination: &Path) -> Result<()>;
    fn delete(&self, destination: &Path) -> Result<()>;
    fn identity(&self, path: &Path) -> Result<Identity> {
        Identity::read(path)
    }
    fn read_only(&self, _: &Path) -> Result<bool> {
        Ok(false)
    }
}

pub(super) struct Btrfs;
#[cfg(target_os = "linux")]
pub(super) fn nested_mounts_from(text: &str, root: &Path) -> Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStringExt;
    let mut result = Vec::new();
    for line in text.lines() {
        let field = line
            .split_whitespace()
            .nth(4)
            .context("invalid Linux mount metadata")?;
        let bytes = field.as_bytes();
        let mut path = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                let digits = bytes
                    .get(index + 1..index + 4)
                    .context("truncated mount path escape")?;
                anyhow::ensure!(
                    digits.iter().all(|c| (b'0'..=b'7').contains(c)),
                    "invalid mount path escape"
                );
                let value = (u16::from(digits[0] - b'0') << 6)
                    | (u16::from(digits[1] - b'0') << 3)
                    | u16::from(digits[2] - b'0');
                anyhow::ensure!(value <= 255, "invalid mount path escape");
                path.push(value as u8);
                index += 4;
            } else {
                path.push(bytes[index]);
                index += 1;
            }
        }
        let path = PathBuf::from(OsString::from_vec(path));
        if path != root && path.starts_with(root) {
            result.push(path);
        }
    }
    Ok(result)
}

impl Backend for Btrfs {
    fn validate(&self, source: &Path, directory: &Path) -> Result<()> {
        let identity = Identity::read(source)?;
        anyhow::ensure!(identity.subvolume_uuid.is_some() && identity.inode == 256,
            "workspace creation requires a Btrfs project subvolume; see the Btrfs workspace setup guide");
        use crate::workspace_snapshot::discovery;
        anyhow::ensure!(discovery::filesystem_id(source)? == discovery::filesystem_id(directory)?,
            "workspace directory must be on the project's Btrfs filesystem; configure workspace init --directory PATH");
        anyhow::ensure!(discovery::descendants(source)?.is_empty(), "nested Btrfs subvolumes require deliberate reconstruction; no partial workspace is created");
        Ok(())
    }
    fn snapshot(&self, source: &Path, destination: &Path) -> Result<()> {
        let result = Command::new("btrfs")
            .args(["subvolume", "snapshot"])
            .arg(source)
            .arg(destination)
            .output()
            .context("install btrfs-progs to create writable workspaces")?;
        anyhow::ensure!(
            result.status.success(),
            "Btrfs writable snapshot failed; inspect permissions and same-filesystem placement"
        );
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no arguments or memory accesses.
            anyhow::ensure!(
                fs::metadata(source)?.uid() == unsafe { libc::geteuid() },
                "workspace source must be owned by the current user"
            );
            let mounts = fs::read_to_string("/proc/self/mountinfo")?;
            anyhow::ensure!(nested_mounts_from(&mounts, source)?.is_empty(), "nested mounts are outside a Btrfs snapshot; choose a project without nested mounts");
        }
        Ok(())
    }
    fn delete(&self, destination: &Path) -> Result<()> {
        let result = Command::new("btrfs")
            .args(["subvolume", "delete"])
            .arg(destination)
            .output()?;
        anyhow::ensure!(result.status.success(), "Btrfs workspace removal failed; deletion may require user_subvol_rm_allowed (no elevation attempted)");
        Ok(())
    }
    fn read_only(&self, path: &Path) -> Result<bool> {
        let output = Command::new("btrfs")
            .args(["property", "get", "-ts"])
            .arg(path)
            .arg("ro")
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "cannot verify checkpoint read-only state"
        );
        Ok(output.stdout == b"ro=true\n")
    }
}

#[derive(Serialize, Deserialize)]
struct Settings {
    format: String,
    directory: PathBuf,
}

pub(super) fn project_key(project: &Identity) -> String {
    project
        .subvolume_uuid
        .clone()
        .unwrap_or_else(|| format!("{:x}-{:x}", project.device, project.inode))
}

pub(super) struct Source {
    pub path: PathBuf,
    pub project: Identity,
    pub logical_root: PathBuf,
    pub checkpoint: Option<CheckpointReference>,
}

impl Registry {
    pub fn configure(&self, directory: &Path) -> Result<()> {
        let _lock = lock(self)?;
        private_directory(directory)?;
        let directory = directory.canonicalize()?;
        crate::workspace_snapshot::discovery::filesystem_id(&directory).context(
            "workspace storage must be on Btrfs; mount the volume before configuring it",
        )?;
        save_json(
            &self.root.join("settings.json"),
            &Settings {
                format: FORMAT.into(),
                directory,
            },
        )
    }

    fn directory(&self) -> Result<PathBuf> {
        let settings = self.root.join("settings.json");
        if settings.exists() {
            let settings: Settings = read_json(&settings)?;
            anyhow::ensure!(
                settings.format == FORMAT,
                "unsupported workspace storage settings"
            );
            anyhow::ensure!(settings.directory.is_dir(), "configured workspace store is missing; mount/reconnect it before retrying (no fallback directory created)");
            return Ok(settings.directory);
        }
        Ok(self
            .root
            .parent()
            .context("workspace registry needs a parent")?
            .join("workspaces"))
    }

    pub(super) fn directory_for(
        &self,
        project: &Identity,
        selected: Option<&Path>,
    ) -> Result<PathBuf> {
        if let Some(directory) = selected {
            return Ok(if directory.is_absolute() {
                directory.to_path_buf()
            } else {
                std::env::current_dir()?.join(directory)
            });
        }
        if let Some(directory) = self
            .records()?
            .into_iter()
            .filter(|record| record.project.same(project))
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
            .and_then(|record| {
                record
                    .workspace
                    .path
                    .parent()
                    .and_then(Path::parent)
                    .map(PathBuf::from)
            })
        {
            anyhow::ensure!(directory.is_dir(), "saved workspace store is missing; mount/reconnect it or select --directory explicitly");
            return Ok(directory);
        }
        self.directory()
    }

    pub fn create(&self, name: &str, source: &Path, directory: Option<&Path>) -> Result<Record> {
        self.create_with(name, source, directory, &Btrfs)
    }

    pub(super) fn create_with(
        &self,
        name: &str,
        source: &Path,
        directory: Option<&Path>,
        backend: &dyn Backend,
    ) -> Result<Record> {
        let logical_root = if source.is_absolute() {
            source.to_path_buf()
        } else {
            std::env::current_dir()?.join(source)
        };
        let source = source.canonicalize().context("source project unavailable; reconnect it or select --from explicitly (no workspace recreated)")?;
        let previous = Self::discover(&source)?;
        let project = previous
            .as_ref()
            .map(|(_, record)| record.project.clone())
            .unwrap_or(Identity::read(&source)?);
        self.create_source_with(
            name,
            Source {
                path: source,
                project,
                logical_root,
                checkpoint: None,
            },
            directory,
            backend,
        )
    }

    pub(super) fn create_source_with(
        &self,
        name: &str,
        source: Source,
        directory: Option<&Path>,
        backend: &dyn Backend,
    ) -> Result<Record> {
        valid_name(name)?;
        let Source {
            path: source,
            project,
            logical_root,
            checkpoint,
        } = source;
        let home = dirs::home_dir();
        validate_project_root(&Identity::read(&source)?, home.as_deref())?;
        validate_project_root(&project, home.as_deref())?;
        let _lock = lock(self)?;
        if let Some(record) = self
            .records()?
            .into_iter()
            .find(|r| r.name == name && r.project.same(&project) && !r.deleted)
        {
            anyhow::ensure!(
                checkpoint.is_none(),
                "recovery needs a new workspace name; the existing workspace is preserved"
            );
            anyhow::ensure!(
                record.workspace.matches(&record.workspace.path),
                "saved workspace is missing/replaced; explicit recovery is required"
            );
            return Ok(record);
        }
        let directory = self.directory_for(&project, directory)?;
        private_directory(&directory)?;
        let directory = directory.canonicalize()?;
        anyhow::ensure!(
            !directory.starts_with(&source) && !self.root.canonicalize()?.starts_with(&source),
            "workspace storage and registry must be outside the source project"
        );
        let minimum = std::env::var("AI_FENCE_WORKSPACE_MIN_FREE_BYTES")
            .unwrap_or_else(|_| "1073741824".into())
            .parse::<u64>()
            .context("AI_FENCE_WORKSPACE_MIN_FREE_BYTES must be a byte count")?;
        anyhow::ensure!(
            fs2::available_space(&directory)? >= minimum,
            "workspace store is below its free-space floor; no automatic deletion attempted"
        );
        backend.validate(&source, &directory)?;
        independent_git(&source)?;
        let parent = directory.join(project_key(&project));
        private_directory(&parent)?;
        let destination = parent.join(name);
        anyhow::ensure!(fs::symlink_metadata(&destination).is_err(), "unregistered workspace destination exists; preserve it and inspect instead of overwriting");
        let source_before = Identity::read(&source)?;
        backend.snapshot(&source, &destination)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))?;
        }
        anyhow::ensure!(
            source_before.matches(&source),
            "source changed during workspace creation; destination retained for inspection"
        );
        independent_git(&destination).with_context(|| {
            format!(
                "new workspace retained at {} for inspection; Git metadata independence failed",
                destination.display()
            )
        })?;
        let record = Record {
            format: FORMAT.into(),
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            project,
            logical_root,
            workspace: Identity::read(&destination)?,
            created_at: chrono::Utc::now().to_rfc3339(),
            backend: "btrfs".into(),
            base_head: head(&destination),
            pinned: false,
            deleted: false,
            chats: Vec::new(),
            native_homes: Vec::new(),
            checkpoints: checkpoint.into_iter().collect(),
            lease: None,
        };
        // The marker is the only workspace-side addition; it contains no account or history data.
        save_json(
            &destination.join(MARKER),
            &Marker {
                format: FORMAT.into(),
                id: record.id.clone(),
                registry: self.root.canonicalize()?,
            },
        )?;
        if destination.join(".git").is_dir() {
            let exclude = destination.join(".git/info/exclude");
            if fs::symlink_metadata(&exclude).is_ok() {
                anyhow::ensure!(
                    !fs::symlink_metadata(&exclude)?.file_type().is_symlink(),
                    "Git exclude file must remain inside the workspace"
                );
            }
            let mut file = OpenOptions::new().create(true).append(true).open(exclude)?;
            writeln!(file, "\n/{MARKER}")?;
        }
        self.save(&record)?;
        Ok(record)
    }

    pub(super) fn remove_with(
        &self,
        name: &str,
        project: Option<&Path>,
        apply: bool,
        discard: bool,
        detach: bool,
        backend: &dyn Backend,
    ) -> Result<serde_json::Value> {
        let _lock = lock(self)?;
        let mut record = self.lookup(name, project)?;
        anyhow::ensure!(!record.deleted, "workspace was already removed");
        let active = record.lease.as_ref().is_some_and(leases::active);
        let inspection = dirty(&record);
        let unverified = inspection.is_err();
        let changed = inspection.unwrap_or(true);
        let linked = !record.chats.is_empty() || !record.native_homes.is_empty();
        let protected = active || record.pinned || (changed && !discard) || (linked && !detach);
        let report = serde_json::json!({"preview": !apply, "name": record.name, "path": record.workspace.path,
            "active": active, "pinned": record.pinned, "changes": changed, "chat_links": record.chats.len(),
            "native_cli_stores": record.native_homes, "inspection_unverified": unverified, "protected": protected});
        if !apply {
            return Ok(report);
        }
        anyhow::ensure!(!protected, "workspace protected: close active sessions; changes require --discard-changes and chat links require --detach-chats; pinned workspaces cannot be removed");
        anyhow::ensure!(
            record.workspace.matches(&record.workspace.path),
            "workspace replaced before removal"
        );
        backend.validate(
            &record.workspace.path,
            record
                .workspace
                .path
                .parent()
                .context("invalid workspace path")?,
        )?;
        backend.delete(&record.workspace.path)?;
        // Retain identity and chat links: a later resume must report removal, never choose another cwd.
        record.deleted = true;
        record.lease = None;
        self.save(&record)?;
        Ok(report)
    }
}
