use super::*;
use std::io::Write;

const FORMAT: &str = "ai-fence-workspace-snapshot-set-v1";

fn valid_id(id: &str) -> bool {
    id.strip_prefix("af-")
        .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
}

fn save(checkpoint: &Checkpoint) -> Result<()> {
    let temporary = checkpoint
        .path
        .join(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(&serde_json::to_vec_pretty(checkpoint)?)?;
        file.sync_all()?;
        fs::rename(&temporary, checkpoint.path.join("manifest.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn free_space_floor(directory: &Path) -> Result<()> {
    let minimum = match std::env::var("AI_FENCE_SNAPSHOT_MIN_FREE_BYTES") {
        Ok(value) => value
            .parse::<u64>()
            .context("invalid AI_FENCE_SNAPSHOT_MIN_FREE_BYTES")?,
        Err(std::env::VarError::NotPresent) => 1024 * 1024 * 1024,
        Err(error) => return Err(error.into()),
    };
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(directory.as_os_str().as_bytes())?;
        let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: path is NUL-terminated and the OS fills the complete statvfs.
        if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot check snapshot-store free space");
        }
        // SAFETY: statvfs succeeded, so its output is initialized.
        let stats = unsafe { stats.assume_init() };
        let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
        if available < u128::from(minimum) {
            bail!("snapshot store is below its {minimum}-byte free-space floor; no automatic deletion attempted");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (directory, minimum);
    Ok(())
}

pub(super) fn create(config: &SnapshotConfig, backend: &dyn Backend) -> Result<Checkpoint> {
    let config = validated_config(config)?;
    backend.validate_filesystems(&config.root, &config.directory)?;
    let _lock = lock_file(&config.directory.join(".ai-fence-snapshots.lock"))?;
    free_space_floor(&config.directory)?;
    let mut sources = vec![config.root.clone()];
    if config.recursive {
        sources.extend(backend.descendants(&config.root)?);
    }
    sources.sort();
    sources.dedup();
    let mut members = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        if canonical_directory(source)? != *source || !source.starts_with(&config.root) {
            bail!("recursive source escaped the approved root");
        }
        members.push(SnapshotMember {
            source_relative_path: source.strip_prefix(&config.root)?.to_path_buf(),
            source_uuid: backend.inspect(source)?,
            snapshot_uuid: None,
            snapshot_name: format!("member-{index:04}"),
        });
    }
    let id = format!("af-{}", uuid::Uuid::new_v4());
    let path = config.directory.join(&id);
    fs::create_dir(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    let mut checkpoint = Checkpoint {
        format: FORMAT.into(), id, created_at: chrono::Utc::now().to_rfc3339(),
        root: config.root.clone(), path, read_only: true,
        git_head: git_output(&config.root, &["rev-parse", "HEAD"]),
        git_common_dir: git_output(&config.root, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).and_then(|path| PathBuf::from(path).canonicalize().ok()),
        nested_subvolumes_included: config.recursive,
        complete: false, pinned: false, error: None,
        consistency: "Members are created sequentially. Quiesce all writers for a consistent cross-subvolume checkpoint.".into(),
        members,
    };
    save(&checkpoint)?;
    let result = (|| {
        for index in 0..checkpoint.members.len() {
            let member = &checkpoint.members[index];
            let source = config.root.join(&member.source_relative_path);
            if backend.inspect(&source)? != member.source_uuid {
                bail!("source subvolume changed during snapshot preparation");
            }
            let uuid = backend.create(&source, &checkpoint.path.join(&member.snapshot_name))?;
            checkpoint.members[index].snapshot_uuid = Some(uuid);
            save(&checkpoint)?;
        }
        if config.recursive {
            let mut after = vec![config.root.clone()];
            after.extend(backend.descendants(&config.root)?);
            after.sort();
            after.dedup();
            if after != sources {
                bail!("nested subvolume membership changed during checkpoint creation");
            }
        }
        for member in &checkpoint.members {
            if backend.inspect(&config.root.join(&member.source_relative_path))?
                != member.source_uuid
            {
                bail!("source subvolume identity changed during checkpoint creation");
            }
        }
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = result {
        checkpoint.error = Some(format!("{error:#}"));
        let _ = save(&checkpoint);
        return Err(error).with_context(|| {
            format!(
                "incomplete checkpoint retained at {}",
                checkpoint.path.display()
            )
        });
    }
    checkpoint.complete = true;
    save(&checkpoint)?;
    Ok(checkpoint)
}

fn load(config: &SnapshotConfig, id: &str) -> Result<Checkpoint> {
    if !valid_id(id) {
        bail!("invalid checkpoint ID");
    }
    let path = config.directory.join(id);
    if canonical_directory(&path)? != path {
        bail!("checkpoint set is a symlink or has moved");
    }
    ensure_owned(&path)?;
    let manifest = path.join("manifest.json");
    ensure_owned(&manifest)?;
    if !fs::symlink_metadata(&manifest)?.is_file() {
        bail!("checkpoint manifest must be a regular file");
    }
    let checkpoint: Checkpoint = serde_json::from_reader(fs::File::open(&manifest)?)?;
    if checkpoint.format != FORMAT
        || checkpoint.id != id
        || checkpoint.path != path
        || checkpoint.root != config.root
    {
        bail!("checkpoint manifest identity does not match the approved workspace/store");
    }
    let mut names = std::collections::HashSet::new();
    for member in &checkpoint.members {
        if !member.snapshot_name.starts_with("member-")
            || member.snapshot_name.len() < 11
            || !member.snapshot_name[7..]
                .chars()
                .all(|c| c.is_ascii_digit())
            || !names.insert(&member.snapshot_name)
            || member
                .source_relative_path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            bail!("checkpoint manifest contains an unsafe or duplicate member");
        }
    }
    Ok(checkpoint)
}

pub(super) fn list(config: &SnapshotConfig) -> Result<Vec<Checkpoint>> {
    let config = validated_config(config)?;
    let mut checkpoints = Vec::new();
    for entry in fs::read_dir(&config.directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().filter(|id| valid_id(id)) else {
            continue;
        };
        let manifest = entry.path().join("manifest.json");
        if !manifest.exists() {
            continue;
        }
        if !fs::symlink_metadata(&manifest)?.is_file() {
            bail!("checkpoint manifest must be a regular file");
        }
        // Store is shared by many approved roots. Read only enough to filter;
        // load() then validates all paths before returning or mutating a set.
        let checkpoint: Checkpoint = serde_json::from_reader(fs::File::open(&manifest)?)
            .with_context(|| format!("invalid checkpoint manifest {}", manifest.display()))?;
        if checkpoint.root == config.root {
            checkpoints.push(load(&config, id)?);
        }
    }
    checkpoints.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(checkpoints)
}

pub fn pin(config: &SnapshotConfig, id: &str, pinned: bool) -> Result<Checkpoint> {
    let config = validated_config(config)?;
    let _lock = lock_file(&config.directory.join(".ai-fence-snapshots.lock"))?;
    let mut checkpoint = load(&config, id)?;
    checkpoint.pinned = pinned;
    save(&checkpoint)?;
    Ok(checkpoint)
}

#[derive(Debug, Serialize)]
pub struct PrunePlan {
    pub applied: bool,
    pub keep_last: usize,
    pub minimum_age_days: u64,
    pub max_sets: usize,
    pub candidates: Vec<String>,
    pub protected: Vec<String>,
    pub note: String,
}

/// Only complete, unpinned sets older than seven days and beyond the newest
/// three for this source are eligible. Dry-run is the default CLI behavior.
pub fn prune(config: &SnapshotConfig, apply: bool) -> Result<PrunePlan> {
    prune_bounded(config, apply, 32)
}

pub fn prune_bounded(config: &SnapshotConfig, apply: bool, max_sets: usize) -> Result<PrunePlan> {
    if !(1..=1000).contains(&max_sets) {
        bail!("prune limit must be 1..=1000");
    }
    prune_with_backend(config, apply, chrono::Utc::now(), max_sets, &Btrfs)
}

fn prune_with_backend(
    config: &SnapshotConfig,
    apply: bool,
    now: chrono::DateTime<chrono::Utc>,
    max_sets: usize,
    backend: &dyn Backend,
) -> Result<PrunePlan> {
    let config = validated_config(config)?;
    backend.validate_filesystems(&config.root, &config.directory)?;
    let _lock = lock_file(&config.directory.join(".ai-fence-snapshots.lock"))?;
    let mut checkpoints = list(&config)?;
    checkpoints.reverse();
    let mut complete_count = 0;
    let mut plan = PrunePlan { applied: apply, keep_last: 3, minimum_age_days: 7, max_sets, candidates: Vec::new(), protected: Vec::new(), note: "Only recorded snapshot sets are eligible. Incomplete and pinned sets are protected. Writable workspaces are never deleted. Physical reclaimed bytes depend on shared extents.".into() };
    for checkpoint in &checkpoints {
        if checkpoint.complete {
            complete_count += 1;
        }
        let age = chrono::DateTime::parse_from_rfc3339(&checkpoint.created_at)?;
        if checkpoint.complete
            && !checkpoint.pinned
            && complete_count > 3
            && now.signed_duration_since(age).num_days() >= 7
        {
            plan.candidates.push(checkpoint.id.clone());
        } else {
            plan.protected.push(checkpoint.id.clone());
        }
    }
    if plan.candidates.len() > max_sets {
        plan.protected.extend(plan.candidates.split_off(max_sets));
    }
    if !apply {
        return Ok(plan);
    }
    // Validate all candidates before deleting anything, including UUID identity.
    for id in &plan.candidates {
        let checkpoint = load(&config, id)?;
        if checkpoint.members.is_empty() {
            bail!("refusing to prune an empty checkpoint set");
        }
        for member in &checkpoint.members {
            let path = checkpoint.path.join(&member.snapshot_name);
            ensure_owned(&path)?;
            if canonical_directory(&path)? != path
                || member.snapshot_uuid.as_ref() != Some(&backend.inspect(&path)?)
                || !backend.read_only(&path)?
            {
                bail!("checkpoint member identity changed; refusing deletion");
            }
        }
        for entry in fs::read_dir(&checkpoint.path)? {
            let name = entry?.file_name();
            if name != "manifest.json"
                && !checkpoint
                    .members
                    .iter()
                    .any(|member| name == member.snapshot_name.as_str())
            {
                bail!("unexpected files inside checkpoint set; refusing deletion");
            }
        }
    }
    for id in &plan.candidates {
        let mut checkpoint = load(&config, id)?;
        checkpoint.complete = false;
        checkpoint.error =
            Some("explicit prune in progress; inspect manually if interrupted".into());
        save(&checkpoint)?;
        for member in &checkpoint.members {
            backend.delete(&checkpoint.path.join(&member.snapshot_name))?;
        }
        fs::remove_file(checkpoint.path.join("manifest.json"))?;
        fs::remove_dir(&checkpoint.path)?;
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    struct Fake {
        fail_on_second: bool,
        creates: Cell<usize>,
    }
    impl Backend for Fake {
        fn validate_filesystems(&self, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }
        fn inspect(&self, path: &Path) -> Result<String> {
            Ok(if path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("member-")
            {
                "snapshot"
            } else {
                "source"
            }
            .into())
        }
        fn descendants(&self, root: &Path) -> Result<Vec<PathBuf>> {
            Ok(vec![root.join("child")])
        }
        fn create(&self, _: &Path, destination: &Path) -> Result<String> {
            self.creates.set(self.creates.get() + 1);
            if self.fail_on_second && self.creates.get() == 2 {
                bail!("injected failure");
            }
            fs::create_dir(destination)?;
            Ok("snapshot".into())
        }
        fn read_only(&self, _: &Path) -> Result<bool> {
            Ok(true)
        }
        fn delete(&self, path: &Path) -> Result<()> {
            fs::remove_dir(path)?;
            Ok(())
        }
    }
    fn fixture() -> (tempfile::TempDir, SnapshotConfig, Fake) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let directory = temp.path().join("snapshots");
        fs::create_dir_all(root.join("child")).unwrap();
        fs::create_dir(&directory).unwrap();
        (
            temp,
            SnapshotConfig {
                root,
                directory,
                recursive: true,
            },
            Fake {
                fail_on_second: false,
                creates: Cell::new(0),
            },
        )
    }
    #[test]
    fn recursive_set_records_children_and_failed_sets_remain_visible() {
        let (_temp, config, mut backend) = fixture();
        backend.fail_on_second = true;
        assert!(create(&config, &backend).is_err());
        let checkpoints = list(&config).unwrap();
        assert_eq!(checkpoints.len(), 1);
        assert!(!checkpoints[0].complete);
        assert!(checkpoints[0].error.as_ref().unwrap().contains("injected"));
        assert_eq!(checkpoints[0].members.len(), 2);
        assert_eq!(
            checkpoints[0].members[1].source_relative_path,
            Path::new("child")
        );
        assert!(checkpoints[0].members[0].snapshot_uuid.is_some());
        assert!(checkpoints[0].members[1].snapshot_uuid.is_none());
    }
    #[test]
    fn prune_protects_recent_pinned_incomplete_and_newest_three() {
        let (_temp, config, backend) = fixture();
        let now = chrono::Utc::now();
        let mut sets = Vec::new();
        for index in 0..7 {
            let mut checkpoint = create(&config, &backend).unwrap();
            checkpoint.created_at = (now - chrono::Duration::days(20 - index)).to_rfc3339();
            save(&checkpoint).unwrap();
            sets.push(checkpoint);
        }
        pin(&config, &sets[0].id, true).unwrap();
        sets[1].complete = false;
        save(&sets[1]).unwrap();
        let plan = prune_with_backend(&config, false, now, 32, &backend).unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(list(&config).unwrap().len(), 7);
        assert!(!plan.candidates.contains(&sets[0].id));
        assert!(!plan.candidates.contains(&sets[1].id));
        prune_with_backend(&config, true, now, 32, &backend).unwrap();
        assert_eq!(list(&config).unwrap().len(), 5);
    }
    #[test]
    fn prune_limits_preview_and_deletion_without_losing_protected_sets() {
        let (_temp, config, backend) = fixture();
        let now = chrono::Utc::now();
        for index in 0..8 {
            let mut checkpoint = create(&config, &backend).unwrap();
            checkpoint.created_at = (now - chrono::Duration::days(20 - index)).to_rfc3339();
            save(&checkpoint).unwrap();
        }
        let plan = prune_with_backend(&config, false, now, 2, &backend).unwrap();
        assert_eq!(plan.candidates.len(), 2);
        assert_eq!(plan.protected.len(), 6);
        assert_eq!(list(&config).unwrap().len(), 8);
        prune_with_backend(&config, true, now, 2, &backend).unwrap();
        assert_eq!(list(&config).unwrap().len(), 6);
    }

    #[test]
    fn filesystem_mismatch_is_rejected_before_any_store_write() {
        struct Mismatch;
        impl Backend for Mismatch {
            fn validate_filesystems(&self, _: &Path, _: &Path) -> Result<()> {
                bail!("different Btrfs filesystems");
            }
            fn inspect(&self, _: &Path) -> Result<String> {
                panic!("filesystem validation must happen first")
            }
            fn create(&self, _: &Path, _: &Path) -> Result<String> {
                panic!("must not write mismatched store")
            }
        }
        let (_temp, config, _) = fixture();
        assert!(create(&config, &Mismatch).is_err());
        assert_eq!(fs::read_dir(&config.directory).unwrap().count(), 0);
    }
    #[test]
    fn tampered_member_path_prevents_pruning() {
        let (_temp, config, backend) = fixture();
        let mut checkpoint = create(&config, &backend).unwrap();
        checkpoint.members[0].snapshot_name = "../../other".into();
        save(&checkpoint).unwrap();
        assert!(list(&config).is_err());
    }
}
