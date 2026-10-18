//! Optional local workspace checkpoints. No proxy, auth, restore, or worktree ownership.
use anyhow::{bail, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

mod command;
pub(crate) mod discovery;
pub use command::{execute, SnapshotAction};
mod sets;
pub use sets::{pin, prune, prune_bounded, PrunePlan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPolicy {
    Auto,
    Off,
    Required,
}

impl SnapshotPolicy {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("off").trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "off" => Ok(Self::Off),
            "required" => Ok(Self::Required),
            _ => bail!("AI_FENCE_SNAPSHOTS must be auto, off, or required"),
        }
    }

    pub fn from_env() -> Result<Self> {
        let value = std::env::var_os("AI_FENCE_SNAPSHOTS")
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("AI_FENCE_SNAPSHOTS must be valid text"))
            })
            .transpose()?;
        Self::parse(value.as_deref())
    }
}

fn off_reason() -> &'static str {
    if std::env::var_os("AI_FENCE_SNAPSHOTS").is_some() {
        "disabled by AI_FENCE_SNAPSHOTS=off"
    } else {
        "automatic checkpoint creation is off by default"
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// An explicitly approved workspace subvolume. Never inferred from ancestors.
    pub root: PathBuf,
    /// Existing directory outside the source, on the same Btrfs filesystem.
    pub directory: PathBuf,
    pub recursive: bool,
}

impl SnapshotConfig {
    pub fn from_env_for(cwd: &Path) -> Result<Option<Self>> {
        let root = match std::env::var_os("AI_FENCE_SNAPSHOT_ROOT") {
            Some(root) => Some(PathBuf::from(root)),
            None => {
                let cwd = cwd.canonicalize()?;
                approved_roots()?
                    .into_iter()
                    .filter_map(|root| root.canonicalize().ok())
                    .filter(|root| cwd.starts_with(root))
                    .max_by_key(|root| root.components().count())
            }
        };
        let Some(root) = root else {
            return Ok(None);
        };
        let directory = std::env::var_os("AI_FENCE_SNAPSHOT_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("AI_FENCE_HOME").map(|path| PathBuf::from(path).join("snapshots"))
            })
            .or_else(|| dirs::home_dir().map(|home| home.join(".ai-fence/snapshots")))
            .context("cannot resolve checkpoint store; set AI_FENCE_SNAPSHOT_DIR")?;
        let recursive = match std::env::var("AI_FENCE_SNAPSHOT_RECURSIVE").as_deref() {
            Ok("1" | "true") => true,
            Ok("0" | "false") | Err(std::env::VarError::NotPresent) => false,
            _ => bail!("AI_FENCE_SNAPSHOT_RECURSIVE must be 1, true, 0, or false"),
        };
        Ok(Some(Self {
            root,
            directory,
            recursive,
        }))
    }
}

fn approved_roots() -> Result<Vec<PathBuf>> {
    #[derive(Deserialize)]
    struct Registry {
        roots: Vec<PathBuf>,
    }
    match std::env::var_os("AI_FENCE_SNAPSHOT_ROOTS_FILE") {
        Some(path) => {
            let registry: Registry = serde_json::from_reader(fs::File::open(path)?)
                .context("invalid AI_FENCE_SNAPSHOT_ROOTS_FILE; expected {\"roots\":[\"/approved/subvolume\"]}")?;
            Ok(registry.roots)
        }
        None => Ok(Vec::new()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMember {
    pub source_relative_path: PathBuf,
    pub source_uuid: String,
    pub snapshot_uuid: Option<String>,
    pub snapshot_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub format: String,
    pub id: String,
    pub created_at: String,
    pub root: PathBuf,
    /// Set directory; its members are read-only subvolumes and manifest.json is metadata.
    pub path: PathBuf,
    pub read_only: bool,
    pub git_head: Option<String>,
    pub git_common_dir: Option<PathBuf>,
    pub nested_subvolumes_included: bool,
    pub complete: bool,
    pub pinned: bool,
    pub error: Option<String>,
    pub consistency: String,
    pub members: Vec<SnapshotMember>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotOutcome {
    pub status: String,
    pub reason: Option<String>,
    pub checkpoint: Option<Checkpoint>,
}

impl SnapshotOutcome {
    fn skipped(status: &str, reason: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            reason: Some(reason.into()),
            checkpoint: None,
        }
    }
}

trait Backend {
    fn validate_filesystems(&self, root: &Path, directory: &Path) -> Result<()> {
        if discovery::filesystem_id(root)? != discovery::filesystem_id(directory)? {
            bail!("workspace and snapshot store must be on the same Btrfs filesystem");
        }
        Ok(())
    }
    fn inspect(&self, root: &Path) -> Result<String>;
    fn create(&self, root: &Path, destination: &Path) -> Result<String>;
    fn descendants(&self, root: &Path) -> Result<Vec<PathBuf>> {
        discovery::descendants(root)
    }
    fn read_only(&self, path: &Path) -> Result<bool> {
        Ok(btrfs(&[
            "property".as_ref(),
            "get".as_ref(),
            "-ts".as_ref(),
            path.as_os_str(),
            "ro".as_ref(),
        ])?
        .trim()
            == "ro=true")
    }
    fn delete(&self, path: &Path) -> Result<()> {
        btrfs(&["subvolume".as_ref(), "delete".as_ref(), path.as_os_str()])?;
        Ok(())
    }
}

struct Btrfs;

fn btrfs(args: &[&std::ffi::OsStr]) -> Result<String> {
    let output = Command::new("btrfs")
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .context("cannot run btrfs; install btrfs-progs on Linux")?;
    if !output.status.success() {
        bail!(
            "btrfs failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl Backend for Btrfs {
    fn inspect(&self, root: &Path) -> Result<String> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if fs::metadata(root)?.ino() != 256 {
                bail!("approved workspace is a directory, not a Btrfs subvolume root");
            }
            discovery::uuid(root)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root;
            bail!("Btrfs workspace checkpoints require Linux")
        }
    }

    fn create(&self, root: &Path, destination: &Path) -> Result<String> {
        btrfs(&[
            "subvolume".as_ref(),
            "snapshot".as_ref(),
            "-r".as_ref(),
            root.as_os_str(),
            destination.as_os_str(),
        ])?;
        let read_only = btrfs(&[
            "property".as_ref(),
            "get".as_ref(),
            "-ts".as_ref(),
            destination.as_os_str(),
            "ro".as_ref(),
        ])?;
        if read_only.trim() != "ro=true" {
            bail!(
                "checkpoint {} was not read-only; retained for inspection",
                destination.display()
            );
        }
        self.inspect(destination).with_context(|| {
            format!(
                "snapshot exists at {}; inspect it manually",
                destination.display()
            )
        })
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "{} must be an existing directory, not a symlink",
            path.display()
        );
    }
    path.canonicalize()
        .with_context(|| format!("cannot resolve {}", path.display()))
}

fn forbidden_workspace_root(
    root: &Path,
    home_root: Option<&Path>,
    user_home: Option<&Path>,
) -> bool {
    root.parent().is_none()
        || root == Path::new("/home")
        || home_root == Some(root)
        || user_home == Some(root)
}

fn validated_config(config: &SnapshotConfig) -> Result<SnapshotConfig> {
    let root = canonical_directory(&config.root)?;
    let directory = canonical_directory(&config.directory)?;
    ensure_owned(&directory)?;
    let home_root = Path::new("/home").canonicalize().ok();
    let user_home = dirs::home_dir().and_then(|home| home.canonicalize().ok());
    if forbidden_workspace_root(&root, home_root.as_deref(), user_home.as_deref()) {
        bail!("refusing to checkpoint an entire home or filesystem; approve a workspace subvolume");
    }
    if directory.starts_with(&root) {
        bail!("snapshot directory must be outside the approved workspace");
    }
    if std::env::var_os("AI_FENCE_SNAPSHOT_ROOTS_FILE").is_some() {
        let allowed = approved_roots()?
            .into_iter()
            .any(|path| path.canonicalize().ok().as_ref() == Some(&root));
        if !allowed {
            bail!("workspace is not in AI_FENCE_SNAPSHOT_ROOTS_FILE");
        }
    }
    Ok(SnapshotConfig {
        root,
        directory,
        recursive: config.recursive,
    })
}

fn ensure_owned(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or side effects.
        let uid = unsafe { libc::geteuid() };
        if fs::symlink_metadata(path)?.uid() != uid {
            bail!(
                "{} is not owned by the effective user; refusing snapshot-store mutation",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn lock_file(path: &Path) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    file.lock_exclusive()?;
    Ok(file)
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Metadata-only inventory for explicit workspace creation and recovery tools.
pub fn inspect_scope(config: &SnapshotConfig) -> Result<serde_json::Value> {
    let config = validated_config(config)?;
    Btrfs.validate_filesystems(&config.root, &config.directory)?;
    let uuid = Btrfs.inspect(&config.root)?;
    let nested = Btrfs.descendants(&config.root)?;
    Ok(serde_json::json!({"root": config.root, "uuid": uuid, "nested": nested}))
}

pub fn status(config: &SnapshotConfig) -> SnapshotOutcome {
    let result = (|| {
        let config = validated_config(config)?;
        Btrfs.inspect(&config.root)?;
        Btrfs.validate_filesystems(&config.root, &config.directory)?;
        let count = if config.recursive {
            Btrfs.descendants(&config.root)?.len() + 1
        } else {
            1
        };
        Ok::<_, anyhow::Error>(count)
    })();
    match result {
        Ok(count) => SnapshotOutcome::skipped("available", format!("{count} approved subvolume(s); recursive sets require quiesced writers for cross-subvolume consistency")),
        Err(error) => SnapshotOutcome::skipped("unavailable", format!("{error:#}")),
    }
}

pub fn create(config: &SnapshotConfig) -> Result<Checkpoint> {
    create_with_backend(config, &Btrfs)
}
fn create_with_backend(config: &SnapshotConfig, backend: &dyn Backend) -> Result<Checkpoint> {
    sets::create(config, backend)
}
pub fn list(config: &SnapshotConfig) -> Result<Vec<Checkpoint>> {
    sets::list(config)
}

/// Run before renting credentials. Off returns before reading paths or invoking any backend.
pub fn prepare_launch(cwd: impl FnOnce() -> Result<PathBuf>) -> Result<SnapshotOutcome> {
    let policy = SnapshotPolicy::from_env()?;
    if policy == SnapshotPolicy::Off {
        return Ok(SnapshotOutcome::skipped("off", off_reason()));
    }
    let cwd = match cwd() {
        Ok(cwd) => cwd,
        Err(error) => return policy_error(policy, error),
    };
    let config = match SnapshotConfig::from_env_for(&cwd) {
        Ok(config) => config,
        Err(error) => return policy_error(policy, error),
    };
    if config.is_none() {
        return if policy == SnapshotPolicy::Required {
            bail!("required checkpoint needs an approved root via AI_FENCE_SNAPSHOT_ROOT or AI_FENCE_SNAPSHOT_ROOTS_FILE")
        } else {
            Ok(SnapshotOutcome::skipped(
                "unconfigured",
                "no workspace subvolume explicitly approved",
            ))
        };
    }
    prepare_with_backend(policy, config.as_ref(), &cwd, &Btrfs)
}

fn policy_error(policy: SnapshotPolicy, error: anyhow::Error) -> Result<SnapshotOutcome> {
    if policy == SnapshotPolicy::Required {
        Err(error.context("required workspace checkpoint failed before launch"))
    } else {
        Ok(SnapshotOutcome::skipped(
            "unavailable",
            format!("{error:#}"),
        ))
    }
}

fn prepare_with_backend(
    policy: SnapshotPolicy,
    config: Option<&SnapshotConfig>,
    cwd: &Path,
    backend: &dyn Backend,
) -> Result<SnapshotOutcome> {
    if policy == SnapshotPolicy::Off {
        return Ok(SnapshotOutcome::skipped("off", off_reason()));
    }
    let result = (|| {
        let config = config.context("no workspace subvolume explicitly approved")?;
        let resolved = validated_config(config)?;
        let cwd = cwd.canonicalize()?;
        if !cwd.starts_with(&resolved.root) {
            bail!("launch directory is outside the explicitly approved workspace");
        }
        if !resolved.recursive
            && backend
                .descendants(&resolved.root)?
                .iter()
                .any(|child| cwd.starts_with(child))
        {
            bail!("launch directory is inside a nested subvolume omitted by this scope; approve that root or enable recursive checkpoints");
        }
        create_with_backend(&resolved, backend)
    })();
    match result {
        Ok(checkpoint) => Ok(SnapshotOutcome {
            status: "created".into(),
            reason: None,
            checkpoint: Some(checkpoint),
        }),
        Err(error) => policy_error(policy, error),
    }
}

/// Export only this launch's checkpoint. Clear stale inherited receipts first.
pub fn configure_command(command: &mut Command, outcome: &SnapshotOutcome) {
    for key in [
        "AI_FENCE_SNAPSHOT_ID",
        "AI_FENCE_SNAPSHOT_PATH",
        "AI_FENCE_SNAPSHOT_RECEIPT",
    ] {
        command.env_remove(key);
    }
    command.env("AI_FENCE_SNAPSHOT_STATUS", &outcome.status);
    if let Some(checkpoint) = &outcome.checkpoint {
        command.env("AI_FENCE_SNAPSHOT_ID", &checkpoint.id);
        command.env("AI_FENCE_SNAPSHOT_PATH", &checkpoint.path);
        command.env(
            "AI_FENCE_SNAPSHOT_RECEIPT",
            checkpoint.path.join("manifest.json"),
        );
        command.env("AI_FENCE_SNAPSHOT_ROOT", &checkpoint.root);
        if let Some(directory) = checkpoint.path.parent() {
            command.env("AI_FENCE_SNAPSHOT_DIR", directory);
        }
    }
}

const GUIDANCE_START: &str = "<!-- ai-fence-workspace-snapshots:start -->";
const GUIDANCE_END: &str = "<!-- ai-fence-workspace-snapshots:end -->";
const GUIDANCE: &str = r#"Workspace checkpoints: inspect AI_FENCE_SNAPSHOT_STATUS. When it is created,
AI_FENCE_SNAPSHOT_ID and AI_FENCE_SNAPSHOT_PATH identify this launch's read-only
checkpoint; AI_FENCE_SNAPSHOT_RECEIPT points to its JSON manifest. Checkpoints
are cheap copy-on-write references, but changed blocks still consume storage.
You may create additional checkpoints without confirmation for an explicitly
approved workspace root. Select TASK_SNAPSHOT_ROOT as the exact configured
AI_FENCE_SNAPSHOT_ROOT, or an exact root listed in AI_FENCE_SNAPSHOT_ROOTS_FILE.
When the allowlist file is configured, the selected root must appear in it.
An app-server started outside a project may have no AI_FENCE_SNAPSHOT_ROOT;
read its configured allowlist and select the task's exact listed workspace root.
An absent startup checkpoint does not prevent checkpoints of allowlisted roots.
If neither source of explicit approval exists, do not create checkpoints.
Use the standalone helper (the global ai-fence-cli may be an older release):
  ai-fence-snapshot status --root "$TASK_SNAPSHOT_ROOT" --directory "$AI_FENCE_SNAPSHOT_DIR"
  ai-fence-snapshot create --root "$TASK_SNAPSHOT_ROOT" --directory "$AI_FENCE_SNAPSHOT_DIR"
  ai-fence-snapshot list --root "$TASK_SNAPSHOT_ROOT" --directory "$AI_FENCE_SNAPSHOT_DIR"
Use --recursive to include nested subvolumes; pause writers for a consistent set.
AI_FENCE_SNAPSHOTS=off disables checkpoint creation. Do not infer permission
by searching filesystem ancestors, snapshot an entire home, restore over active
work, delete checkpoints, or treat a checkpoint as independent Git worktree
isolation. External Git common directories remain outside its scope.
"#;

/// Stable guidance in the actual managed native home. Never replace user instructions.
pub fn install_codex_guidance(codex_home: &Path) -> Result<()> {
    let _lock = lock_file(&codex_home.join(".ai-fence-snapshot-guidance.lock"))?;
    let path = codex_home.join("AGENTS.md");
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        bail!(
            "refusing to replace non-file Codex instructions at {}",
            path.display()
        );
    }
    let existing = match fs::read_to_string(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let block = format!("{GUIDANCE_START}\n{GUIDANCE}{GUIDANCE_END}");
    let updated = match (existing.find(GUIDANCE_START), existing.find(GUIDANCE_END)) {
        (Some(start), Some(end)) if start < end => format!(
            "{}{}{}",
            &existing[..start],
            block,
            &existing[end + GUIDANCE_END.len()..]
        ),
        (None, None) => format!("{existing}\n\n{block}\n"),
        _ => bail!(
            "incomplete ai-fence snapshot guidance markers in {}; preserve and repair manually",
            path.display()
        ),
    };
    if updated != existing {
        let temporary = codex_home.join(format!(".AGENTS-{}.tmp", uuid::Uuid::new_v4()));
        fs::write(&temporary, updated)?;
        if let Ok(metadata) = fs::metadata(&path) {
            fs::set_permissions(&temporary, metadata.permissions())?;
        }
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(error).context("cannot atomically update Codex checkpoint guidance");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct FakeBackend {
        calls: Cell<usize>,
        fail: bool,
    }
    impl Backend for FakeBackend {
        fn validate_filesystems(&self, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }
        fn descendants(&self, _: &Path) -> Result<Vec<PathBuf>> {
            Ok(Vec::new())
        }
        fn inspect(&self, _: &Path) -> Result<String> {
            self.calls.set(self.calls.get() + 1);
            if self.fail {
                bail!("unsupported filesystem");
            }
            Ok("source-uuid".into())
        }
        fn create(&self, _: &Path, destination: &Path) -> Result<String> {
            self.calls.set(self.calls.get() + 1);
            fs::create_dir(destination)?;
            Ok("snapshot-uuid".into())
        }
    }
    fn fixture() -> (tempfile::TempDir, SnapshotConfig) {
        let temp = tempfile::tempdir().unwrap();
        let config = SnapshotConfig {
            root: temp.path().join("workspace with spaces"),
            directory: temp.path().join("checkpoints"),
            recursive: false,
        };
        fs::create_dir(&config.root).unwrap();
        fs::create_dir(&config.directory).unwrap();
        (temp, config)
    }
    #[test]
    fn off_never_probes_even_with_missing_paths() {
        let backend = FakeBackend::default();
        let config = SnapshotConfig {
            root: "/missing".into(),
            directory: "/missing".into(),
            recursive: false,
        };
        assert_eq!(
            prepare_with_backend(
                SnapshotPolicy::Off,
                Some(&config),
                Path::new("/missing"),
                &backend
            )
            .unwrap()
            .status,
            "off"
        );
        assert_eq!(backend.calls.get(), 0);
    }
    #[test]
    fn policies_are_explicit() {
        assert_eq!(SnapshotPolicy::parse(None).unwrap(), SnapshotPolicy::Off);
        assert_eq!(
            SnapshotPolicy::parse(Some("off")).unwrap(),
            SnapshotPolicy::Off
        );
        assert!(SnapshotPolicy::parse(Some("false")).is_err());
    }
    #[test]
    fn unsupported_auto_skips_and_required_fails() {
        let (_temp, config) = fixture();
        let backend = FakeBackend {
            fail: true,
            ..Default::default()
        };
        assert_eq!(
            prepare_with_backend(SnapshotPolicy::Auto, Some(&config), &config.root, &backend)
                .unwrap()
                .status,
            "unavailable"
        );
        assert!(prepare_with_backend(
            SnapshotPolicy::Required,
            Some(&config),
            &config.root,
            &backend
        )
        .is_err());
        assert_eq!(backend.calls.get(), 2);
    }
    #[test]
    fn refuses_outside_scope_and_nested_destination_without_probe() {
        let (temp, mut config) = fixture();
        let backend = FakeBackend::default();
        assert!(prepare_with_backend(
            SnapshotPolicy::Required,
            Some(&config),
            temp.path(),
            &backend
        )
        .is_err());
        config.directory = config.root.clone();
        assert!(create_with_backend(&config, &backend).is_err());
        assert_eq!(backend.calls.get(), 0);
    }
    #[test]
    fn manifests_are_scoped_and_unique() {
        let (_temp, config) = fixture();
        let backend = FakeBackend::default();
        let first = create_with_backend(&config, &backend).unwrap();
        let second = create_with_backend(&config, &backend).unwrap();
        assert_ne!(first.id, second.id);
        assert!(first.read_only);
        assert!(!first.nested_subvolumes_included);
        assert_eq!(list(&config).unwrap().len(), 2);
    }
    #[test]
    fn plain_directory_is_unavailable_without_ancestor_discovery() {
        let (_temp, config) = fixture();
        let result = status(&config);
        assert_eq!(result.status, "unavailable");
        assert!(result.reason.unwrap().contains("subvolume root"));
    }
    #[test]
    fn guidance_preserves_user_text_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let agents = temp.path().join("AGENTS.md");
        fs::write(&agents, "My existing rules.\n").unwrap();
        install_codex_guidance(temp.path()).unwrap();
        let first = fs::read_to_string(&agents).unwrap();
        install_codex_guidance(temp.path()).unwrap();
        assert_eq!(first, fs::read_to_string(agents).unwrap());
        assert!(first.starts_with("My existing rules."));
        assert_eq!(first.matches(GUIDANCE_START).count(), 1);
    }
    #[test]
    fn guidance_upgrade_replaces_obsolete_command_and_preserves_surrounding_user_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("AGENTS.md");
        fs::write(&path, format!("User rules before.\n{GUIDANCE_START}\nai-fence-cli snapshot create obsolete\n{GUIDANCE_END}\nUser rules after.\n")).unwrap();
        install_codex_guidance(temp.path()).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with("User rules before.\n"));
        assert!(updated.ends_with("\nUser rules after.\n"));
        assert!(!updated.contains("ai-fence-cli snapshot create"));
        assert!(updated.contains("ai-fence-snapshot create --root"));
        assert!(updated.contains("AI_FENCE_SNAPSHOT_ROOTS_FILE"));
        assert!(updated.contains(
            "An app-server started outside a project may have no AI_FENCE_SNAPSHOT_ROOT"
        ));
        assert!(updated.contains("Use --recursive"));
        install_codex_guidance(temp.path()).unwrap();
        assert_eq!(updated, fs::read_to_string(path).unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let (temp, mut config) = fixture();
        let target = config.root.clone();
        config.root = temp.path().join("root-link");
        symlink(&target, &config.root).unwrap();
        assert!(validated_config(&config).is_err());
        let target_file = temp.path().join("rules");
        fs::write(&target_file, "untouched").unwrap();
        symlink(&target_file, target.join("AGENTS.md")).unwrap();
        assert!(install_codex_guidance(&target).is_err());
        assert_eq!(fs::read_to_string(target_file).unwrap(), "untouched");
    }
    #[test]
    fn nonrecursive_launch_inside_child_subvolume_is_rejected() {
        struct Nested;
        impl Backend for Nested {
            fn descendants(&self, root: &Path) -> Result<Vec<PathBuf>> {
                Ok(vec![root.join("child")])
            }
            fn inspect(&self, _: &Path) -> Result<String> {
                panic!("must reject omitted scope before snapshot probing")
            }
            fn create(&self, _: &Path, _: &Path) -> Result<String> {
                panic!("must not create misleading baseline")
            }
        }
        let (_temp, config) = fixture();
        let child = config.root.join("child");
        fs::create_dir(&child).unwrap();
        let outcome =
            prepare_with_backend(SnapshotPolicy::Auto, Some(&config), &child, &Nested).unwrap();
        assert_eq!(outcome.status, "unavailable");
        assert!(outcome.reason.unwrap().contains("nested subvolume omitted"));
    }
    #[test]
    fn concurrent_creates_leave_complete_unique_manifests() {
        let (_temp, config) = fixture();
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let config = config.clone();
                std::thread::spawn(move || {
                    create_with_backend(&config, &FakeBackend::default())
                        .unwrap()
                        .id
                })
            })
            .collect();
        let ids: std::collections::HashSet<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(ids.len(), 4);
        assert!(list(&config)
            .unwrap()
            .iter()
            .all(|checkpoint| checkpoint.complete));
    }
    #[test]
    fn malformed_guidance_markers_preserve_existing_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let text = format!("User text\n{GUIDANCE_START}\nunfinished");
        let path = temp.path().join("AGENTS.md");
        fs::write(&path, &text).unwrap();
        assert!(install_codex_guidance(temp.path()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
    #[cfg(unix)]
    #[test]
    fn guidance_preserves_permissions_and_trailing_user_whitespace() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("AGENTS.md");
        let text = "User rules.  \n\n";
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        install_codex_guidance(temp.path()).unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with(text));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    #[cfg(unix)]
    #[test]
    fn intermediate_home_symlink_keeps_project_and_store_paths_canonical() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let physical = temp.path().join("physical-home");
        let project = physical.join("matthid/proj/project");
        let store = physical.join("matthid/.ai-fence/snapshots");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&store).unwrap();
        let logical = temp.path().join("logical-home");
        symlink(&physical, &logical).unwrap();
        let logical_root = logical.join("matthid/proj/project");
        let config = SnapshotConfig {
            root: logical_root.clone(),
            directory: logical.join("matthid/.ai-fence/snapshots"),
            recursive: false,
        };
        let resolved = validated_config(&config).unwrap();
        assert_eq!(resolved.root, project);
        assert_eq!(resolved.directory, store);
        // The same canonical comparison is used for explicit registry entries.
        assert_eq!(logical_root.canonicalize().unwrap(), resolved.root);
        let canonical_home = logical.canonicalize().unwrap();
        let canonical_user = logical.join("matthid").canonicalize().unwrap();
        assert!(forbidden_workspace_root(
            &canonical_home,
            Some(&canonical_home),
            Some(&canonical_user)
        ));
        assert!(forbidden_workspace_root(
            &canonical_user,
            Some(&canonical_home),
            Some(&canonical_user)
        ));
        assert!(!forbidden_workspace_root(
            &resolved.root,
            Some(&canonical_home),
            Some(&canonical_user)
        ));
    }
    #[test]
    fn command_clears_inherited_checkpoint_metadata() {
        let mut command = Command::new("unused");
        command.env("AI_FENCE_SNAPSHOT_ID", "old");
        configure_command(&mut command, &SnapshotOutcome::skipped("off", "disabled"));
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "AI_FENCE_SNAPSHOT_ID" && value.is_none()));
    }
}
