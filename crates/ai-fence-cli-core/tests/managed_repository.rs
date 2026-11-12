#![cfg(target_os = "linux")]
use ai_fence_cli_core::workspace::{
    repository::Generation,
    source::{self, Query},
    Identity, Registry,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn git(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().to_owned()
}
fn initialize(path: &Path) {
    fs::create_dir_all(path).unwrap();
    git(path, &["init", "-b", "master"]);
    fs::write(path.join("hello.rs"), "fn first() {}\n").unwrap();
    fs::write(path.join(".env"), "PRIVATE=fixture\n").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", path.join("outside")).unwrap();
    git(path, &["add", "."]);
    git(path, &["commit", "-m", "first"]);
}

#[test]
fn committed_source_reads_ignore_dirty_files_and_exclude_symlinks_and_secret_paths() {
    let root = tempfile::tempdir().unwrap();
    initialize(root.path());
    let generation = Generation {
        id: uuid::Uuid::new_v4().to_string(),
        commit: git(root.path(), &["rev-parse", "HEAD"]),
        created_at: chrono::Utc::now().to_rfc3339(),
        snapshot: Identity::read(root.path()).unwrap(),
    };
    fs::write(root.path().join("hello.rs"), "dirty private contents").unwrap();
    let listing = source::read(
        &generation,
        Query::List {
            prefix: String::new(),
            after: String::new(),
        },
    )
    .unwrap();
    assert_eq!(listing["data"]["files"], serde_json::json!(["hello.rs"]));
    let value = source::read(
        &generation,
        Query::Read {
            path: "hello.rs".into(),
            start_line: 1,
        },
    )
    .unwrap();
    assert_eq!(value["data"]["lines"][0], "fn first() {}");
    for path in ["outside", ".env", ".git/config", "../secret", "/etc/passwd"] {
        assert!(source::read(
            &generation,
            Query::Read {
                path: path.into(),
                start_line: 1
            }
        )
        .is_err());
    }
    let value = source::read(
        &generation,
        Query::Search {
            literal: "first".into(),
            prefix: String::new(),
        },
    )
    .unwrap();
    assert_eq!(value["data"]["matches"][0]["path"], "hello.rs");
    git(root.path(), &["add", "hello.rs"]);
    git(root.path(), &["commit", "-m", "changed"]);
    assert!(source::read(
        &generation,
        Query::List {
            prefix: String::new(),
            after: String::new()
        }
    )
    .is_err());
}

struct BtrfsFixture {
    directory: tempfile::TempDir,
    subvolumes: Vec<PathBuf>,
}
impl Drop for BtrfsFixture {
    fn drop(&mut self) {
        for path in self.subvolumes.iter().rev() {
            assert!(path.starts_with(self.directory.path()));
            // Unprivileged Btrfs deletion checks the victim inode's write
            // permission. Only disposable fixture generations are unsealed.
            let status = Command::new("btrfs")
                .args(["property", "set", "-ts"])
                .arg(path)
                .args(["ro", "false"])
                .status()
                .unwrap();
            assert!(status.success());
            let status = Command::new("btrfs")
                .args(["subvolume", "delete"])
                .arg(path)
                .status()
                .unwrap();
            assert!(
                status.success(),
                "owned test subvolume cleanup failed: {}",
                path.display()
            );
        }
    }
}

#[test]
#[ignore = "requires explicit AI_FENCE_TEST_BTRFS_DIR; run for local host qualification"]
fn real_btrfs_refresh_preserves_dirty_workspaces_and_independent_git() {
    let environment = vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ];
    let root =
        std::env::var_os("AI_FENCE_TEST_BTRFS_DIR").expect("explicit disposable Btrfs test root");
    let mut fixture = BtrfsFixture {
        directory: tempfile::tempdir_in(root).unwrap(),
        subvolumes: Vec::new(),
    };
    let root = fixture.directory.path().to_path_buf();
    let remote = root.join("upstream");
    initialize(&remote);
    let registry = Registry {
        root: root.join("registry"),
    };
    let id = uuid::Uuid::new_v4().to_string();
    let repository = registry
        .repository_ensure(
            &id,
            remote.to_str().unwrap(),
            "master",
            Some(&root.join("store")),
            &environment,
        )
        .unwrap();
    fixture.subvolumes.push(repository.master.path.clone());
    fixture
        .subvolumes
        .push(repository.current.snapshot.path.clone());
    let old_generation = repository.current.clone();
    let master_identity = repository.master.clone();
    let workspace = registry.repository_workspace(&id, "working").unwrap();
    fixture.subvolumes.push(workspace.workspace.path.clone());
    fs::write(
        workspace.workspace.path.join("hello.rs"),
        "dirty work survives\n",
    )
    .unwrap();
    fs::write(remote.join("hello.rs"), "fn second() {}\n").unwrap();
    git(&remote, &["add", "hello.rs"]);
    git(&remote, &["commit", "-m", "second"]);
    let refreshed = registry.repository_refresh(&id, &environment).unwrap();
    fixture
        .subvolumes
        .push(refreshed.current.snapshot.path.clone());
    assert!(master_identity.same(&refreshed.master));
    assert_ne!(old_generation.id, refreshed.current.id);
    assert_eq!(
        git(
            &refreshed.master.path,
            &["rev-parse", "refs/remotes/origin/master"]
        ),
        refreshed.current.commit
    );
    assert_eq!(
        fs::read_to_string(workspace.workspace.path.join("hello.rs")).unwrap(),
        "dirty work survives\n"
    );
    let resumed = registry.repository_workspace(&id, "working").unwrap();
    assert_eq!(resumed.id, workspace.id);
    let newer = registry.repository_workspace(&id, "newer").unwrap();
    fixture.subvolumes.push(newer.workspace.path.clone());
    assert_eq!(
        fs::read_to_string(newer.workspace.path.join("hello.rs")).unwrap(),
        "fn second() {}\n"
    );
    assert!(newer.workspace.path.join(".git").is_dir());
    assert!(!newer
        .workspace
        .path
        .join(".git/objects/info/alternates")
        .exists());
    assert_ne!(
        git(&workspace.workspace.path, &["rev-parse", "HEAD"]),
        git(&newer.workspace.path, &["rev-parse", "HEAD"])
    );
    fs::write(
        refreshed.master.path.join("hello.rs"),
        "unexpected dirty master",
    )
    .unwrap();
    assert!(registry.repository_refresh(&id, &environment).is_err());
    assert_eq!(
        registry.repository(&id).unwrap().current.id,
        refreshed.current.id
    );
    assert!(registry
        .repository_ensure(
            &id,
            "https://example.invalid/other.git",
            "master",
            None,
            &[]
        )
        .is_err());
    let old = source::read(
        &old_generation,
        Query::Read {
            path: "hello.rs".into(),
            start_line: 1,
        },
    )
    .unwrap();
    assert_eq!(old["data"]["lines"][0], "fn first() {}");
    let delta = registry
        .repository_source(
            &id,
            None,
            Query::Diff {
                base_generation_id: old_generation.id.clone(),
                prefix: String::new(),
            },
            "diff-reader",
            &uuid::Uuid::new_v4().to_string(),
            chrono::Utc::now() + chrono::Duration::minutes(10),
        )
        .unwrap();
    assert_eq!(
        delta["data"]["changed_files"],
        serde_json::json!(["hello.rs"])
    );
    for pin in delta["view_lease_ids"].as_array().unwrap() {
        registry
            .repository_unpin(&id, pin.as_str().unwrap(), "diff-reader")
            .unwrap();
    }
    let failed_id = uuid::Uuid::new_v4().to_string();
    for _ in 0..2 {
        assert!(registry
            .repository_ensure(
                &failed_id,
                remote.to_str().unwrap(),
                "missing-branch",
                Some(&root.join("store")),
                &environment
            )
            .is_err());
        assert!(!root.join("store").join(&failed_id).exists());
    }
    let pin = uuid::Uuid::new_v4().to_string();
    registry
        .repository_pin(
            &id,
            &old_generation.id,
            &pin,
            "reader-a",
            chrono::Utc::now() + chrono::Duration::minutes(10),
            false,
        )
        .unwrap();
    assert!(registry.repository_unpin(&id, &pin, "reader-b").is_err());
    assert_eq!(
        registry.repository_prune(&id, 0, 0, true).unwrap()["generation_ids"],
        serde_json::json!([])
    );
    registry.repository_unpin(&id, &pin, "reader-a").unwrap();
    let removed = registry.repository_prune(&id, 0, 0, true).unwrap();
    assert_eq!(
        removed["generation_ids"],
        serde_json::json!([old_generation.id])
    );
    fixture
        .subvolumes
        .retain(|path| path != &old_generation.snapshot.path);
    assert!(!old_generation.snapshot.path.exists());
    assert_eq!(
        fs::read_to_string(workspace.workspace.path.join("hello.rs")).unwrap(),
        "dirty work survives\n"
    );
    assert!(registry
        .repository_generation(&id, Some(&old_generation.id))
        .is_err());
    assert_eq!(
        registry.repository(&id).unwrap().current.id,
        refreshed.current.id
    );
}
