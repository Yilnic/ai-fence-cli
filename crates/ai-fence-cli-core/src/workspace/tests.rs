use super::storage::Backend;
use super::*;
use std::process::Command;

struct FixtureBackend;
impl storage::Backend for FixtureBackend {
    fn validate(&self, _: &Path, _: &Path) -> Result<()> {
        Ok(())
    }
    fn snapshot(&self, source: &Path, destination: &Path) -> Result<()> {
        fn copy(from: &Path, to: &Path) -> Result<()> {
            fs::create_dir(to)?;
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    copy(&entry.path(), &to.join(entry.file_name()))?;
                } else {
                    fs::copy(entry.path(), to.join(entry.file_name()))?;
                }
            }
            Ok(())
        }
        copy(source, destination)
    }
    fn delete(&self, destination: &Path) -> Result<()> {
        Ok(fs::remove_dir_all(destination)?)
    }
}

struct Fixture {
    _temporary: tempfile::TempDir,
    registry: Registry,
    project: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        fs::create_dir(&project).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&project)
            .status()
            .unwrap()
            .success());
        fs::write(project.join("tracked.txt"), "initial\n").unwrap();
        let run = |args: &[&str]| assert!(storage::git(&project, args).unwrap().status.success());
        run(&["add", "."]);
        run(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ]);
        let registry = Registry {
            root: temporary.path().join("registry"),
        };
        let store = temporary.path().join("store");
        Self {
            _temporary: temporary,
            registry,
            project,
            store,
        }
    }
    fn create(&self, name: &str) -> Record {
        self.registry
            .create_with(name, &self.project, Some(&self.store), &FixtureBackend)
            .unwrap()
    }
}

#[test]
fn clones_preserve_dirty_untracked_and_git_independence_without_recloning() {
    let fixture = Fixture::new();
    fs::write(fixture.project.join("tracked.txt"), "unsaved source\n").unwrap();
    fs::write(fixture.project.join("untracked.txt"), "local file\n").unwrap();
    let first = fixture.create("feature-a");
    fs::write(first.workspace.path.join("tracked.txt"), "agent edit\n").unwrap();
    let second = fixture.create("feature-a");
    assert_eq!(first.id, second.id);
    assert_eq!(
        fs::read_to_string(second.workspace.path.join("tracked.txt")).unwrap(),
        "agent edit\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.project.join("tracked.txt")).unwrap(),
        "unsaved source\n"
    );
    assert!(second.workspace.path.join("untracked.txt").exists());
    assert!(storage::dirty(&second).unwrap());
    storage::independent_git(&second.workspace.path).unwrap();
}

#[test]
fn physical_aliases_reuse_project_names_and_cd_discovers_subdirectories() {
    let fixture = Fixture::new();
    let first = fixture.create("feature-a");
    #[cfg(unix)]
    {
        let alias = fixture._temporary.path().join("alias");
        std::os::unix::fs::symlink(&fixture.project, &alias).unwrap();
        let second = fixture
            .registry
            .create_with("feature-a", &alias, Some(&fixture.store), &FixtureBackend)
            .unwrap();
        assert_eq!(first.id, second.id);
    }
    let directory = first.workspace.path.join("nested");
    fs::create_dir(&directory).unwrap();
    assert_eq!(
        Registry::discover(&directory).unwrap().unwrap().1.id,
        first.id
    );
}

#[test]
fn concurrent_creation_is_idempotent_and_keeps_independent_names() {
    let fixture = Fixture::new();
    std::thread::scope(|scope| {
        let results: Vec<_> = (0..6)
            .map(|_| scope.spawn(|| fixture.create("same")))
            .collect();
        let ids: Vec<_> = results.into_iter().map(|v| v.join().unwrap().id).collect();
        assert!(ids.iter().all(|id| id == &ids[0]));
    });
    let a = fixture.create("same");
    let b = fixture.create("other");
    assert_ne!(a.id, b.id);
    assert!(!a.workspace.same(&b.workspace));
}

#[test]
fn names_and_existing_unowned_destinations_cannot_escape_or_overwrite() {
    let fixture = Fixture::new();
    for name in ["", "../escape", "/absolute", "a/b", "--flag"] {
        assert!(fixture
            .registry
            .create_with(
                name,
                &fixture.project,
                Some(&fixture.store),
                &FixtureBackend
            )
            .is_err());
    }
    fixture.create("first");
    let parent = fs::read_dir(&fixture.store)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::create_dir(parent.join("unowned")).unwrap();
    assert!(fixture
        .registry
        .create_with(
            "unowned",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
    assert!(parent.join("unowned").exists());
}

#[test]
fn missing_or_replaced_workspace_is_never_recreated_on_resume_or_create() {
    let fixture = Fixture::new();
    let record = fixture.create("missing");
    fs::rename(
        &record.workspace.path,
        record.workspace.path.with_extension("saved"),
    )
    .unwrap();
    assert!(fixture
        .registry
        .create_with(
            "missing",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
    assert!(!record.workspace.path.exists());
    fs::create_dir(&record.workspace.path).unwrap();
    assert!(fixture
        .registry
        .create_with(
            "missing",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
}

#[test]
fn cleanup_preview_protects_dirty_active_pinned_and_chat_linked_work() {
    let fixture = Fixture::new();
    let mut record = fixture.create("clean");
    assert!(!storage::dirty(&record).unwrap());
    let preview = fixture
        .registry
        .remove_with(
            "clean",
            Some(&fixture.project),
            false,
            false,
            false,
            &FixtureBackend,
        )
        .unwrap();
    assert_eq!(preview["preview"], true);
    assert!(record.workspace.path.exists());
    fs::write(record.workspace.path.join("untracked"), "keep").unwrap();
    assert!(fixture
        .registry
        .remove_with(
            "clean",
            Some(&fixture.project),
            true,
            false,
            false,
            &FixtureBackend
        )
        .is_err());
    record.pinned = true;
    fixture.registry.save(&record).unwrap();
    assert!(fixture
        .registry
        .remove_with(
            "clean",
            Some(&fixture.project),
            true,
            true,
            true,
            &FixtureBackend
        )
        .is_err());
    assert!(record.workspace.path.join("untracked").exists());
}

#[test]
fn committed_and_ignored_work_is_also_protected_from_removal() {
    let fixture = Fixture::new();
    let record = fixture.create("commits");
    fs::write(record.workspace.path.join("tracked.txt"), "new work").unwrap();
    assert!(storage::git(&record.workspace.path, &["add", "."])
        .unwrap()
        .status
        .success());
    assert!(storage::git(
        &record.workspace.path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "new work"
        ]
    )
    .unwrap()
    .status
    .success());
    assert!(storage::dirty(&record).unwrap());
    let ignored = fixture.create("ignored");
    fs::write(
        ignored.workspace.path.join(".git/info/exclude"),
        format!("/{MARKER}\nsecret-local\n"),
    )
    .unwrap();
    fs::write(
        ignored.workspace.path.join("secret-local"),
        "fixture ignored work",
    )
    .unwrap();
    assert!(storage::dirty(&ignored).unwrap());
}

#[test]
fn external_git_alternates_and_nested_repositories_are_refused_before_creation() {
    let fixture = Fixture::new();
    fs::write(
        fixture.project.join(".git/objects/info/alternates"),
        "/external/objects\n",
    )
    .unwrap();
    assert!(fixture
        .registry
        .create_with(
            "alternates",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
    fs::remove_file(fixture.project.join(".git/objects/info/alternates")).unwrap();
    fs::create_dir_all(fixture.project.join("nested/.git")).unwrap();
    assert!(fixture
        .registry
        .create_with(
            "nested",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
}

#[test]
fn git_write_locks_and_uninitialized_submodules_are_refused_before_snapshotting() {
    let fixture = Fixture::new();
    fs::write(fixture.project.join(".git/index.lock"), "fixture Git write").unwrap();
    assert!(fixture
        .registry
        .create_with(
            "locked",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
    fs::remove_file(fixture.project.join(".git/index.lock")).unwrap();
    let head = storage::head(&fixture.project).unwrap();
    let index_entry = format!("160000,{head},uninitialized");
    assert!(storage::git(
        &fixture.project,
        &["update-index", "--add", "--cacheinfo", &index_entry]
    )
    .unwrap()
    .status
    .success());
    assert!(fixture
        .registry
        .create_with(
            "submodules",
            &fixture.project,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
}

#[test]
fn removal_preview_protects_unverified_git_state_but_explicit_discard_can_remove_owned_copy() {
    let fixture = Fixture::new();
    let record = fixture.create("stale");
    fs::write(
        record.workspace.path.join(".git/index.lock"),
        "fixture abandoned write",
    )
    .unwrap();
    let preview = fixture
        .registry
        .remove_with(
            "stale",
            Some(&fixture.project),
            false,
            false,
            false,
            &FixtureBackend,
        )
        .unwrap();
    assert_eq!(preview["inspection_unverified"], true);
    assert_eq!(preview["protected"], true);
    assert!(fixture
        .registry
        .remove_with(
            "stale",
            Some(&fixture.project),
            true,
            false,
            false,
            &FixtureBackend
        )
        .is_err());
    fixture
        .registry
        .remove_with(
            "stale",
            Some(&fixture.project),
            true,
            true,
            false,
            &FixtureBackend,
        )
        .unwrap();
    assert!(fixture.project.exists());
    assert!(!record.workspace.path.exists());
}

#[test]
fn removal_retains_a_tombstone_and_never_silently_reuses_chat_identity() {
    let fixture = Fixture::new();
    let record = fixture.create("done");
    fixture
        .registry
        .remove_with(
            "done",
            Some(&fixture.project),
            true,
            false,
            false,
            &FixtureBackend,
        )
        .unwrap();
    assert!(!record.workspace.path.exists());
    assert!(fixture.registry.read(&record.id).unwrap().deleted);
    let new = fixture.create("done");
    assert_ne!(new.id, record.id);
    assert!(fixture.registry.read(&record.id).unwrap().deleted);
}

#[cfg(target_os = "linux")]
#[test]
fn leases_allow_same_owner_and_reject_pid_reuse_and_other_launches() {
    let fixture = Fixture::new();
    let record = fixture.create("leased");
    let first = fixture
        .registry
        .attach(&record.workspace.path, std::process::id(), None, None)
        .unwrap()
        .unwrap();
    assert!(first.owned);
    let again = fixture
        .registry
        .attach(&record.workspace.path, std::process::id(), None, None)
        .unwrap()
        .unwrap();
    assert!(!again.owned);
    assert_eq!(first.lease, again.lease);
    let preview = fixture
        .registry
        .remove_with(
            "leased",
            Some(&fixture.project),
            false,
            true,
            true,
            &FixtureBackend,
        )
        .unwrap();
    assert_eq!(preview["active"], true);
    let mut current = fixture.registry.read(&record.id).unwrap();
    current.lease.as_mut().unwrap().owner.start = "wrong-start-identity".into();
    fixture.registry.save(&current).unwrap();
    let replacement = fixture
        .registry
        .attach(&record.workspace.path, std::process::id(), None, None)
        .unwrap()
        .unwrap();
    assert!(replacement.owned);
    assert_ne!(replacement.lease, first.lease);
}

#[cfg(target_os = "linux")]
#[test]
fn chat_links_keep_native_homes_separate_and_block_missing_workspace_fallback() {
    let fixture = Fixture::new();
    let record = fixture.create("chat");
    let a = fixture._temporary.path().join("state-a");
    let b = fixture._temporary.path().join("state-b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    let first = Chat::new("thread-old".into(), "personal".into(), &a).unwrap();
    fixture
        .registry
        .attach(
            &record.workspace.path,
            std::process::id(),
            None,
            Some(first.clone()),
        )
        .unwrap();
    assert_eq!(
        fixture.registry.chat(&first).unwrap().unwrap().id,
        record.id
    );
    assert!(fixture
        .registry
        .chat(&Chat::new("thread-old".into(), "personal".into(), &b).unwrap())
        .unwrap()
        .is_none());
    fs::rename(
        &record.workspace.path,
        record.workspace.path.with_extension("saved"),
    )
    .unwrap();
    assert!(fixture.registry.chat(&first).is_err());
}

#[cfg(unix)]
#[test]
fn protected_project_roots_match_aliases_and_missing_recovery_paths() {
    let fixture = Fixture::new();
    let home = fixture._temporary.path().join("home-alias");
    std::os::unix::fs::symlink(&fixture.project, &home).unwrap();
    let identity = Identity::read(&fixture.project).unwrap();
    assert!(validate_project_root(&identity, Some(&home)).is_err());

    // A saved checkpoint identity remains forbidden even when its old alias is
    // gone: recovery must compare the recorded identity with the live home.
    let mut saved = identity.clone();
    saved.path = fixture._temporary.path().join("missing-old-alias");
    assert!(!saved.path.exists());
    assert!(validate_project_root(&saved, Some(&home)).is_err());

    let child = fixture.project.join("approved-project");
    fs::create_dir(&child).unwrap();
    assert!(validate_project_root(&Identity::read(&child).unwrap(), Some(&home)).is_ok());
    assert!(validate_project_root(&Identity::read(Path::new("/")).unwrap(), None).is_err());
    saved.path = PathBuf::from("/home");
    assert!(validate_project_root(&saved, None).is_err());
}

#[test]
fn project_remembers_custom_storage_and_does_not_recreate_a_missing_store() {
    let fixture = Fixture::new();
    let first = fixture.create("first");
    let second = fixture
        .registry
        .create_with("second", &fixture.project, None, &FixtureBackend)
        .unwrap();
    assert_eq!(
        first.workspace.path.parent(),
        second.workspace.path.parent()
    );
    fs::rename(&fixture.store, fixture.store.with_extension("offline")).unwrap();
    assert!(fixture
        .registry
        .create_with("third", &fixture.project, None, &FixtureBackend)
        .is_err());
    assert!(!fixture.store.exists());
}

#[test]
fn new_from_a_workspace_defaults_to_original_project_and_explicit_from_can_fork() {
    let fixture = Fixture::new();
    let workspace = fixture.create("first");
    assert_eq!(
        command::creation_source(&workspace.workspace.path, None).unwrap(),
        fixture.project
    );
    assert_eq!(
        command::creation_source(&workspace.workspace.path, Some(Path::new("."))).unwrap(),
        PathBuf::from(".")
    );
}

#[test]
fn malformed_saved_uuid_cannot_become_a_workspace_storage_path() {
    let fixture = Fixture::new();
    let mut record = fixture.create("valid");
    record.project.subvolume_uuid = Some("../../escape".into());
    fixture.registry.save(&record).unwrap();
    assert!(fixture.registry.read(&record.id).is_err());
    assert!(fixture
        .registry
        .create_with(
            "second",
            &record.workspace.path,
            Some(&fixture.store),
            &FixtureBackend
        )
        .is_err());
    assert!(!fixture._temporary.path().join("escape").exists());
}

#[test]
fn checkpoint_marker_retains_original_project_identity_after_workspace_removal() {
    let fixture = Fixture::new();
    let mut record = fixture.create("old");
    let source_uuid = uuid::Uuid::new_v4().to_string();
    record.workspace.subvolume_uuid = Some(source_uuid.clone());
    record.workspace.inode = 256;
    record.deleted = true;
    fixture.registry.save(&record).unwrap();
    let origin = recovery::original_project(&record.workspace.path, &source_uuid)
        .unwrap()
        .unwrap();
    assert!(origin.same(&record.project));
    assert!(
        recovery::original_project(&record.workspace.path, &uuid::Uuid::new_v4().to_string())
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn nested_mount_detection_decodes_spaces_and_does_not_confuse_siblings() {
    let mounts = "1 0 0:1 / /project\\040one rw - btrfs device rw\n2 1 0:2 / /project\\040one/mounted rw - ext4 device rw\n3 1 0:3 / /project\\040one-other rw - ext4 device rw\n";
    assert_eq!(
        storage::nested_mounts_from(mounts, Path::new("/project one")).unwrap(),
        vec![PathBuf::from("/project one/mounted")]
    );
    assert!(storage::nested_mounts_from("1 0 0:1 / /bad\\999 rw", Path::new("/bad")).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn native_cli_usage_protects_history_without_inventing_thread_ids() {
    let fixture = Fixture::new();
    let record = fixture.create("cli");
    let state = fixture._temporary.path().join("native-state");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("auth.json"), "fixture native refresh").unwrap();
    let guard = acquire_for_directory(&record.workspace.path)
        .unwrap()
        .unwrap();
    guard.register_native_home(&state, "personal").unwrap();
    drop(guard);
    let saved = fixture.registry.read(&record.id).unwrap();
    assert!(saved.chats.is_empty());
    assert_eq!(saved.native_homes.len(), 1);
    assert!(fixture
        .registry
        .remove_with(
            "cli",
            Some(&fixture.project),
            true,
            false,
            false,
            &FixtureBackend
        )
        .is_err());
    fixture
        .registry
        .remove_with(
            "cli",
            Some(&fixture.project),
            true,
            false,
            true,
            &FixtureBackend,
        )
        .unwrap();
    assert_eq!(
        fs::read_to_string(state.join("auth.json")).unwrap(),
        "fixture native refresh"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn known_native_resume_requires_the_saved_workspace_and_selected_state_store() {
    let fixture = Fixture::new();
    let record = fixture.create("linked");
    let state = fixture._temporary.path().join("state");
    fs::create_dir(&state).unwrap();
    let chat = Chat::new("thread-old".into(), "personal".into(), &state).unwrap();
    fixture
        .registry
        .attach(&record.workspace.path, std::process::id(), None, Some(chat))
        .unwrap();
    // Discovery supplies the registry from the marker even without shell variables.
    validate_cli_resume(&record.workspace.path, &state, "personal", "thread-old").unwrap();
    assert!(fixture
        .registry
        .chat(&Chat::new("thread-old".into(), "other-profile".into(), &state).unwrap())
        .unwrap()
        .is_none());
}

#[test]
fn explicit_recovery_creates_a_new_workspace_and_keeps_later_work() {
    use crate::workspace_snapshot::{Checkpoint, SnapshotMember};
    struct RecoveryBackend {
        root: PathBuf,
        snapshot: PathBuf,
        root_uuid: String,
        snapshot_uuid: String,
    }
    impl storage::Backend for RecoveryBackend {
        fn validate(&self, _: &Path, _: &Path) -> Result<()> {
            Ok(())
        }
        fn snapshot(&self, source: &Path, destination: &Path) -> Result<()> {
            FixtureBackend.snapshot(source, destination)
        }
        fn delete(&self, path: &Path) -> Result<()> {
            FixtureBackend.delete(path)
        }
        fn read_only(&self, _: &Path) -> Result<bool> {
            Ok(true)
        }
        fn identity(&self, path: &Path) -> Result<Identity> {
            let mut identity = Identity::read(path)?;
            identity.inode = 256;
            identity.subvolume_uuid = Some(if path == self.root {
                self.root_uuid.clone()
            } else {
                self.snapshot_uuid.clone()
            });
            Ok(identity)
        }
    }
    let fixture = Fixture::new();
    let current = fixture.create("current");
    let set = fixture._temporary.path().join("checkpoint");
    fs::create_dir(&set).unwrap();
    let snapshot = set.join("member-0000");
    FixtureBackend
        .snapshot(&fixture.project, &snapshot)
        .unwrap();
    fs::write(
        current.workspace.path.join("tracked.txt"),
        "later dirty work",
    )
    .unwrap();
    let backend = RecoveryBackend {
        root: fixture.project.clone(),
        snapshot: snapshot.clone(),
        root_uuid: uuid::Uuid::new_v4().to_string(),
        snapshot_uuid: uuid::Uuid::new_v4().to_string(),
    };
    let checkpoint = Checkpoint {
        format: "ai-fence-workspace-snapshot-set-v1".into(),
        id: format!("af-{}", uuid::Uuid::new_v4()),
        created_at: chrono::Utc::now().to_rfc3339(),
        root: fixture.project.clone(),
        path: set.clone(),
        read_only: true,
        git_head: None,
        git_common_dir: None,
        nested_subvolumes_included: false,
        complete: true,
        pinned: false,
        error: None,
        consistency: "fixture".into(),
        members: vec![SnapshotMember {
            source_relative_path: PathBuf::new(),
            source_uuid: backend.root_uuid.clone(),
            snapshot_uuid: Some(backend.snapshot_uuid.clone()),
            snapshot_name: "member-0000".into(),
        }],
    };
    let manifest = set.join("manifest.json");
    storage::save_json(&manifest, &checkpoint).unwrap();
    let source = recovery::source(&manifest, &backend).unwrap();
    assert_eq!(source.path, backend.snapshot);
    let restored = fixture
        .registry
        .create_source_with("rescue", source, Some(&fixture.store), &backend)
        .unwrap();
    assert_ne!(restored.id, current.id);
    assert_eq!(
        fs::read_to_string(restored.workspace.path.join("tracked.txt")).unwrap(),
        "initial\n"
    );
    assert_eq!(
        fs::read_to_string(current.workspace.path.join("tracked.txt")).unwrap(),
        "later dirty work"
    );
    assert_eq!(restored.checkpoints[0].id, checkpoint.id);
    let mut invalid = checkpoint;
    invalid.complete = false;
    storage::save_json(&manifest, &invalid).unwrap();
    assert!(recovery::source(&manifest, &backend).is_err());
    invalid.complete = true;
    invalid.members[0].snapshot_uuid = Some(uuid::Uuid::new_v4().to_string());
    storage::save_json(&manifest, &invalid).unwrap();
    assert!(recovery::source(&manifest, &backend).is_err());
}
