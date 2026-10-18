use std::process::Command;

#[test]
fn off_status_never_probes_missing_paths_or_binaries() {
    let output = Command::new(env!("CARGO_BIN_EXE_ai-fence-snapshot"))
        .env("AI_FENCE_SNAPSHOTS", "off")
        .env("PATH", "/nonexistent")
        .args(["status", "--root", "/missing", "--directory", "/missing"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["status"], "off");
    assert!(output.stderr.is_empty());
}

#[test]
fn off_create_fails_before_path_or_helper_checks() {
    let output = Command::new(env!("CARGO_BIN_EXE_ai-fence-snapshot"))
        .env("AI_FENCE_SNAPSHOTS", "off")
        .env("PATH", "/nonexistent")
        .args(["create", "--root", "/missing", "--directory", "/missing"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("disabled by AI_FENCE_SNAPSHOTS=off"));
    assert!(output.stdout.is_empty());
}

#[test]
fn invalid_policy_is_diagnostic_and_does_not_create_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("uncreated");
    let output = Command::new(env!("CARGO_BIN_EXE_ai-fence-snapshot"))
        .env("AI_FENCE_SNAPSHOTS", "oops")
        .args(["create", "--root"])
        .arg(&root)
        .arg("--directory")
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be auto, off, or required"));
    assert!(!root.exists());
}

#[test]
fn explicit_create_opts_in_when_automatic_checkpoints_default_to_off() {
    let output = Command::new(env!("CARGO_BIN_EXE_ai-fence-snapshot"))
        .env_remove("AI_FENCE_SNAPSHOTS")
        .env_remove("AI_FENCE_SNAPSHOT_ROOTS_FILE")
        .env("PATH", "/nonexistent")
        .args([
            "create",
            "--root",
            "/missing-explicit-root",
            "--directory",
            "/missing-explicit-store",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("disabled by AI_FENCE_SNAPSHOTS=off"),
        "an explicit create command is already opt-in; only an explicit off override should block it");
    assert!(output.stdout.is_empty());
}
