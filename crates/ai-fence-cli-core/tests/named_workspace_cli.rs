//! Exercise actual process/exec boundaries with synthetic owned metadata. No
//! filesystem snapshot backend is faked in the production binary or invoked.
#![cfg(target_os = "linux")]
use ai_fence_cli_core::workspace::{Identity, Record};
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Fixture {
    _directory: tempfile::TempDir,
    registry: PathBuf,
    project: PathBuf,
    workspace: PathBuf,
    native_home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let registry = root.join("registry with spaces");
        let project = root.join("project");
        let workspace = root.join("workspaces/alpha");
        let native_home = root.join("native-home");
        for path in [
            &project,
            &workspace,
            &native_home,
            &registry.join("records"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(native_home.join("auth.json"), "fixture auth stays native").unwrap();
        let fixture = Self {
            _directory: directory,
            registry,
            project,
            workspace,
            native_home,
        };
        fixture.register("alpha", &fixture.workspace);
        fixture
    }

    fn register(&self, name: &str, workspace: &Path) {
        fs::create_dir_all(workspace).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let record = Record {
            format: "ai-fence-named-workspace-v1".into(),
            id: id.clone(),
            name: name.into(),
            project: Identity::read(&self.project).unwrap(),
            logical_root: self.project.clone(),
            workspace: Identity::read(workspace).unwrap(),
            created_at: chrono::Utc::now().to_rfc3339(),
            backend: "btrfs".into(),
            base_head: None,
            pinned: false,
            deleted: false,
            chats: vec![],
            native_homes: vec![],
            checkpoints: vec![],
            lease: None,
        };
        fs::write(
            self.registry.join("records").join(format!("{id}.json")),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        fs::write(
            workspace.join(".ai-fence-workspace.json"),
            serde_json::to_vec(&json!({
                "format":"ai-fence-named-workspace-v1","id":id,"registry":self.registry,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ai-fence-workspace"));
        command
            .current_dir(&self.project)
            .env("AI_FENCE_WORKSPACE_HOME", &self.registry)
            .env("AI_FENCE_CLI_AUTO_UPDATE", "never")
            .env("AI_FENCE_SNAPSHOTS", "off")
            .env_remove("AI_FENCE_WORKSPACE_LEASE")
            .env("CODEX_HOME", &self.native_home)
            .env("AI_FENCE_CODEX_PROFILE", "fixture-account")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn listing(&self) -> serde_json::Value {
        let output = self.command().args(["ls", "--json"]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn run_preserves_native_arguments_homes_stdio_and_exact_exit_status() {
    let fixture = Fixture::new();
    let script = "import os,sys,json; print(json.dumps({'argv':sys.argv[1:],'cwd':os.getcwd(),'home':os.environ['CODEX_HOME'],'profile':os.environ['AI_FENCE_CODEX_PROFILE']})); sys.exit(23)";
    let output = fixture
        .command()
        .args([
            "run",
            "alpha",
            "--",
            "python3",
            "-c",
            script,
            "--yolo",
            "--help",
            "--model",
            "fixture model",
            "",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["argv"],
        json!(["--yolo", "--help", "--model", "fixture model", ""])
    );
    assert_eq!(value["cwd"], fixture.workspace.to_str().unwrap());
    assert_eq!(value["home"], fixture.native_home.to_str().unwrap());
    assert_eq!(value["profile"], "fixture-account");
    assert_eq!(
        fs::read_to_string(fixture.native_home.join("auth.json")).unwrap(),
        "fixture auth stays native"
    );
    assert_eq!(fixture.listing()[0]["active"], false);
}

#[test]
fn concurrent_launches_are_blocked_per_workspace_but_different_names_run() {
    let fixture = Fixture::new();
    fixture.register("beta", &fixture.workspace.with_file_name("beta"));
    let mut first = fixture
        .command()
        .args([
            "run",
            "alpha",
            "--",
            "python3",
            "-u",
            "-c",
            "import sys;print('ready');sys.stdin.read()",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(first.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    assert_eq!(ready.trim(), "ready");
    assert_eq!(fixture.listing()[0]["active"], true);
    let blocked = fixture
        .command()
        .args(["run", "alpha", "--", "true"])
        .output()
        .unwrap();
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("active in another launch"));
    assert!(fixture
        .command()
        .args(["run", "beta", "--", "true"])
        .status()
        .unwrap()
        .success());
    first.stdin.take();
    assert!(first.wait().unwrap().success());
    assert!(fixture
        .command()
        .args(["run", "alpha", "--", "true"])
        .status()
        .unwrap()
        .success());
}

#[test]
fn sigterm_and_failed_exec_leave_no_active_lease_or_helper_process() {
    let fixture = Fixture::new();
    let failed = fixture
        .command()
        .args(["run", "alpha", "--", "/does/not/exist"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(fixture.listing()[0]["active"], false);
    let mut child = fixture
        .command()
        .args([
            "run",
            "alpha",
            "--",
            "python3",
            "-u",
            "-c",
            "import sys;print('ready');sys.stdin.read()",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    // SAFETY: this test owns and has not reaped child, so its PID cannot be reused.
    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    child.wait().unwrap();
    assert_eq!(fixture.listing()[0]["active"], false);
    assert!(fixture
        .command()
        .args(["run", "alpha", "--", "true"])
        .status()
        .unwrap()
        .success());
}

#[test]
fn nested_fenced_style_exec_borrows_the_same_process_owned_lease() {
    let fixture = Fixture::new();
    let output = fixture
        .command()
        .args([
            "run",
            "alpha",
            "--",
            env!("CARGO_BIN_EXE_ai-fence-workspace"),
            "run",
            "alpha",
            "--",
            "pwd",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        fixture.workspace.to_str().unwrap()
    );
}

#[test]
fn shell_function_can_cd_in_parent_shell_then_use_existing_aliases() {
    let fixture = Fixture::new();
    // This fixture executes Bash, regardless of the runner's parent shell.
    // PowerShell module paths can also be inherited by Bash processes.
    let script = fixture
        .command()
        .args(["shell-init", "--shell", "bash"])
        .env("PSModulePath", "synthetic-inherited-powershell-modules")
        .output()
        .unwrap();
    assert!(script.status.success());
    let script = format!("{}\nfws alpha\npwd\nshopt -s expand_aliases\nalias existing_agent='printf alias-preserved'\nexisting_agent\n", String::from_utf8(script.stdout).unwrap());
    let output = Command::new("bash")
        .args(["--noprofile", "--norc", "-c", &script])
        .current_dir(&fixture.project)
        .env("AI_FENCE_WORKSPACE_HOME", &fixture.registry)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8(output.stdout).unwrap();
    assert!(output.starts_with(fixture.workspace.to_str().unwrap()));
    assert!(output.contains("alias-preserved"));
    assert_eq!(
        fixture.listing()[0]["active"],
        false,
        "cd itself should not acquire an indefinite shell lease"
    );
}

#[test]
fn shell_switch_never_interprets_workspace_names_as_commands() {
    let fixture = Fixture::new();
    let script = fixture
        .command()
        .args(["shell-init", "--shell", "bash"])
        .output()
        .unwrap();
    assert!(script.status.success());
    for name in ["ls", "new", "run", "rm", "status"] {
        let path = fixture._directory.path().join("workspaces").join(name);
        fixture.register(name, &path);
        let script = format!(
            "{}\nfws {} || exit\npwd\nfw path {}\n",
            String::from_utf8_lossy(&script.stdout),
            name,
            name
        );
        let output = Command::new("bash")
            .args(["--noprofile", "--norc", "-c", &script])
            .current_dir(&fixture.project)
            .env("AI_FENCE_WORKSPACE_HOME", &fixture.registry)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{0}\n{0}\n", path.display())
        );
    }
}

#[test]
fn shell_setup_initializes_a_selected_profile_once_without_touching_the_project() {
    let fixture = Fixture::new();
    let profile = fixture._directory.path().join("test home/.bashrc");
    fs::create_dir_all(profile.parent().unwrap()).unwrap();
    fs::write(&profile, "# retained custom profile\n").unwrap();
    let helpers = fixture._directory.path().join("helpers with spaces");
    for _ in 0..2 {
        let output = fixture
            .command()
            .args(["shell-setup", "--shell", "bash", "--profile"])
            .arg(&profile)
            .arg("--directory")
            .arg(&helpers)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let profile_text = fs::read_to_string(&profile).unwrap();
    assert!(profile_text.starts_with("# retained custom profile\n"));
    assert_eq!(profile_text.matches("# >>> AI Fence workspace").count(), 1);
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#". "$1"; fws alpha; pwd"#,
            "fixture",
        ])
        .arg(&profile)
        .current_dir(&fixture.project)
        .env("AI_FENCE_WORKSPACE_HOME", &fixture.registry)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        fixture.workspace.to_str().unwrap()
    );
    assert!(!fixture.project.join(".bashrc").exists());
}

#[test]
fn windows_wsl_bridge_preserves_argument_boundaries_cwd_and_exit_status() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let bin = fixture._directory.path().join("bridge-bin");
    fs::create_dir(&bin).unwrap();
    let executable = bin.join("wsl.exe");
    fs::write(&executable, "#!/bin/sh\nprintf '%s\\0' \"$@\"\nexit 23\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let output = fixture
        .command()
        .args([
            "_wsl",
            "--",
            "run",
            "alpha",
            "--",
            "codex-fenced",
            "a b",
            "$(never-execute)",
            "",
            "--yolo",
        ])
        .env("PATH", &bin)
        .env("AI_FENCE_WORKSPACE_WSL_DISTRO", "Fixture Distribution")
        .env("AI_FENCE_WORKSPACE_WSL_CLI", "/some path/cli")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    let values: Vec<_> = output
        .stdout
        .split(|v| *v == 0)
        .map(|v| String::from_utf8_lossy(v).into_owned())
        .collect();
    assert_eq!(
        &values[..6],
        &[
            "--distribution",
            "Fixture Distribution",
            "--cd",
            fixture.project.to_str().unwrap(),
            "--exec",
            "/bin/sh"
        ]
    );
    assert_eq!(
        &values[values.len() - 10..],
        &[
            "/some path/cli",
            "run",
            "alpha",
            "--",
            "codex-fenced",
            "a b",
            "$(never-execute)",
            "",
            "--yolo",
            ""
        ]
    );
}

#[test]
fn path_is_machine_readable_and_missing_workspace_does_not_fallback() {
    let fixture = Fixture::new();
    let path = fixture.command().args(["path", "alpha"]).output().unwrap();
    assert!(path.status.success());
    assert!(path.stderr.is_empty());
    assert_eq!(
        String::from_utf8(path.stdout).unwrap(),
        format!("{}\n", fixture.workspace.display())
    );
    fs::rename(
        &fixture.workspace,
        fixture.workspace.with_file_name("saved-alpha"),
    )
    .unwrap();
    let missing = fixture
        .command()
        .args(["run", "alpha", "--", "pwd"])
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(!fixture.workspace.exists());
}

#[test]
fn rm_is_a_preview_and_protects_untracked_work_without_running_btrfs() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("untracked.txt"), "keep this work").unwrap();
    let output = fixture.command().args(["rm", "alpha"]).output().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["preview"], true);
    assert_eq!(report["protected"], true);
    let denied = fixture
        .command()
        .args(["rm", "alpha", "--apply"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("untracked.txt")).unwrap(),
        "keep this work"
    );
}

#[test]
fn missing_marker_fails_managed_status_and_pin_can_be_changed_explicitly() {
    let fixture = Fixture::new();
    assert!(fixture
        .command()
        .args(["pin", "alpha"])
        .status()
        .unwrap()
        .success());
    assert_eq!(fixture.listing()[0]["pinned"], true);
    assert!(fixture
        .command()
        .args(["pin", "alpha", "--unpin"])
        .status()
        .unwrap()
        .success());
    assert_eq!(fixture.listing()[0]["pinned"], false);
    fs::remove_file(fixture.workspace.join(".ai-fence-workspace.json")).unwrap();
    let missing = fixture
        .command()
        .args(["status", "--path"])
        .arg(&fixture.workspace)
        .output()
        .unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("marker is missing"));
}

#[test]
fn home_alias_is_refused_before_creating_workspace_storage() {
    let fixture = Fixture::new();
    let alias = fixture._directory.path().join("home-alias");
    std::os::unix::fs::symlink(&fixture.native_home, &alias).unwrap();
    let store = fixture._directory.path().join("unused-store");
    let output = fixture
        .command()
        .env("HOME", &alias)
        .args(["create", "unsafe-home", "--from"])
        .arg(&fixture.native_home)
        .arg("--directory")
        .arg(&store)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("entire home"));
    assert!(!store.exists());
}
