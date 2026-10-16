"""Metadata/lifecycle regressions. All files/processes belong to temporary tests."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from types import SimpleNamespace
from unittest.mock import patch

SOURCE = Path(__file__).resolve().parents[2] / "assets/codex_desktop.py"
spec = importlib.util.spec_from_file_location("desktop", SOURCE)
desktop = importlib.util.module_from_spec(spec)
spec.loader.exec_module(desktop)


class DesktopTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="af-desktop-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "state"
        self.home.mkdir()
        self.project = self.root / "project"
        self.project.mkdir()
        self.settings = dict(format=1, profile="fixture", state_home=str(self.home), roots=[],
                             snapshot_policy="off", snapshot_directory=None, install_root=str(self.root / "install"))
        self.lease = dict(pid=os.getpid(), start=desktop.process_identity(os.getpid()))

    def test_resume_keeps_original_cwd_and_dirty_files_with_snapshots_enabled(self):
        self.settings["snapshot_policy"] = "auto"
        dirty = self.project / "untracked.txt"
        dirty.write_text("unsaved work")
        first = desktop.attach(self.settings, "thread_old", self.project, self.lease)
        for _ in range(2):
            params = dict(threadId="thread_old")
            desktop.preflight_resume(self.settings, params)
            self.assertEqual(params["cwd"], str(self.project))
            resumed = desktop.attach(self.settings, "thread_old", self.project, self.lease)
            self.assertEqual(first["workspace"], resumed["workspace"])
        self.assertEqual(dirty.read_text(), "unsaved work")

    def test_logical_and_physical_aliases_match_without_path_constants(self):
        alias = self.root / "alias"
        alias.symlink_to(self.project, target_is_directory=True)
        self.assertEqual(desktop.identity(alias), desktop.identity(self.project))
        desktop.attach(self.settings, "thread_alias", alias, self.lease)
        parameters = dict(threadId="thread_alias")
        desktop.preflight_resume(self.settings, parameters)
        self.assertEqual(parameters["cwd"], str(alias))
        desktop.attach(self.settings, "thread_alias", self.project, self.lease)

    def test_missing_workspace_and_replaced_inode_require_recovery(self):
        desktop.attach(self.settings, "thread_missing", self.project, self.lease)
        self.project.rmdir()
        with self.assertRaisesRegex(RuntimeError, "missing"):
            desktop.preflight_resume(self.settings, dict(threadId="thread_missing"))
        self.project.mkdir()
        with self.assertRaisesRegex(RuntimeError, "another workspace"):
            # An existing different project is never silently selected.
            desktop.attach(self.settings, "thread_missing", self.home, self.lease)

    def test_concurrent_resumes_are_serialized_without_lost_leases(self):
        def attach(_):
            return desktop.attach(self.settings, "thread_shared", self.project, self.lease)
        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(attach, range(24)))
        record = desktop.load(desktop.record_path(self.settings, "thread_shared"))
        self.assertEqual(len(results), 24)
        self.assertEqual(record["leases"], [self.lease])
        other = subprocess.Popen([sys.executable, "-c", "import sys;sys.stdin.read()"], stdin=subprocess.PIPE)
        self.addCleanup(other.stdin.close)
        self.addCleanup(other.wait)
        self.addCleanup(other.terminate)
        foreign = dict(pid=other.pid, start=desktop.process_identity(other.pid))
        with self.assertRaisesRegex(RuntimeError, "active lease"):
            desktop.attach(self.settings, "thread_shared", self.project, foreign)

    def test_app_server_events_register_multiple_new_and_resumed_threads(self):
        lifecycle = desktop.Lifecycle(self.settings, self.lease)
        for index, method in enumerate(["thread/start", "thread/resume", "thread/fork"]):
            lifecycle.request(dict(id=index, method=method, params={}))
            lifecycle.response(dict(id=index, result=dict(thread=dict(id=f"thread_{index}", cwd=str(self.project)))))
        self.assertEqual(len(list((self.home / "ai-fence-workspaces").glob("*.json"))), 3)

    def test_explicit_current_project_selected_instead_of_startup_snapshot(self):
        self.settings.update(snapshot_policy="auto", roots=[str(self.project)], snapshot_directory=str(self.root / "store"))
        calls = []
        def run(argv, **kwargs):
            calls.append((argv, kwargs))
            return SimpleNamespace(returncode=0, stdout=b'{"id":"af-fixture","path":"/fixture"}')
        with patch.dict(os.environ, {"AI_FENCE_SNAPSHOT_ROOT": "/stale-startup-root"}), patch.object(desktop.subprocess, "run", side_effect=run):
            result = desktop.checkpoint(self.settings, self.project, "thread_checkpoint")
        self.assertEqual(result["id"], "af-fixture")
        self.assertIn(str(self.project), calls[0][0])
        self.assertNotIn("AI_FENCE_SNAPSHOT_ROOT", calls[0][1]["env"])
        self.assertIn("pin", calls[1][0])
        self.assertIsNone(desktop.checkpoint(self.settings, self.home, "thread_other"))

    def test_off_never_probes_snapshot_registry(self):
        self.settings.update(snapshot_policy="off", roots=["/does-not-exist"])
        with patch.dict(os.environ, {"AI_FENCE_SNAPSHOTS":"off"}), patch.object(desktop.subprocess, "run", side_effect=AssertionError("must not probe")):
            self.assertIsNone(desktop.checkpoint(self.settings, "/does-not-exist", "thread_off"))

    def test_cleanup_is_bounded_preview_and_protects_chat_linked_dirty_workspaces(self):
        for index in range(3):
            desktop.attach(self.settings, f"thread_{index}", self.project, self.lease)
        (self.project / "untracked").write_text("keep")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            desktop.workspace_control(self.settings, SimpleNamespace(workspace_action="cleanup", limit=2))
        report = json.loads(output.getvalue())
        self.assertTrue(report["preview"])
        self.assertTrue(report["truncated"])
        self.assertTrue(all(v["protected"] == "chat-linked" for v in report["workspaces"]))
        self.assertEqual((self.project / "untracked").read_text(), "keep")

    def test_adoption_preview_does_not_touch_native_history_or_records(self):
        (self.home / "native.sqlite").write_bytes(b"untouched")
        args = SimpleNamespace(workspace_action="adopt", thread_id="old_thread", project=self.project, workspace=None, apply=False)
        with patch.object(desktop, "native_thread", return_value=dict(cwd=str(self.project))), contextlib.redirect_stdout(io.StringIO()):
            desktop.workspace_control(self.settings, args)
        self.assertFalse(desktop.record_path(self.settings, "old_thread").exists())
        self.assertEqual((self.home / "native.sqlite").read_bytes(), b"untouched")

    def test_external_git_metadata_not_advertised_as_isolated(self):
        subprocess.run(["git", "init", "-q", str(self.project)], check=True)
        self.assertTrue(desktop.git_independent(self.project))
        alternate = self.project / ".git/objects/info/alternates"
        alternate.write_text("/external/objects\n")
        self.assertFalse(desktop.git_independent(self.project))

    def native(self):
        native = self.root / "native-codex"
        native.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "codex-cli 0.152.0"; else printf "%s\\n" "$@"; fi\n')
        native.chmod(0o700)
        return native

    def install(self, native=None, cli=None):
        native = native or self.native()
        cli = cli or native
        args = SimpleNamespace(install_root=self.root / "install", native_codex=native,
                               state_home=self.home, seed_home=None, profile="fixture", snapshots="off",
                               approve_root=[], snapshot_directory=None, readiness_seconds=0)
        # A separate fake payload keeps the native/runtime recursion guard meaningful.
        if cli == native:
            cli = self.root / "fence-cli"
            cli.write_text('#!/bin/sh\nexit 0\n')
            cli.chmod(0o700)
        with patch.dict(os.environ, {"AI_FENCE_DESKTOP_CLI_SOURCE": str(cli)}), contextlib.redirect_stdout(io.StringIO()):
            desktop.install(args)
        return desktop.load(args.install_root / "settings.json"), args

    def test_official_setup_idempotence_upgrade_state_preservation_and_uninstall(self):
        (self.home / "auth.json").write_text("fixture-only-auth")
        (self.home / "history.jsonl").write_text("fixture-only-history")
        native = self.native()
        before = desktop.digest(native)
        settings, args = self.install(native)
        again, _ = self.install(native)
        self.assertEqual(settings, again)
        cli = self.root / "fence-cli"
        cli.write_text('#!/bin/sh\n# version two\nexit 0\n')
        updated, _ = self.install(native, cli)
        self.assertNotEqual(settings["release"], updated["release"])
        self.assertTrue((args.install_root / "releases" / settings["release"]).is_dir())
        self.assertEqual((self.home / "auth.json").read_text(), "fixture-only-auth")
        self.assertEqual((self.home / "history.jsonl").read_text(), "fixture-only-history")
        self.assertEqual(desktop.digest(native), before)
        with contextlib.redirect_stdout(io.StringIO()):
            desktop.uninstall(updated, False)
        self.assertTrue((args.install_root / "bin/codex").exists())
        with contextlib.redirect_stdout(io.StringIO()):
            desktop.uninstall(updated, True)
        self.assertTrue(native.exists())
        self.assertTrue((self.home / "history.jsonl").exists())

    def test_native_and_session_only_optout_keep_selected_history_home(self):
        settings, args = self.install()
        shim = args.install_root / "bin/codex"
        result = subprocess.run([str(shim), "--version"], capture_output=True, timeout=5)
        self.assertEqual(result.stdout, b"codex-cli 0.152.0\n")
        env = dict(os.environ, AI_FENCE_CODEX_DESKTOP="0")
        result = subprocess.run([str(shim), "app-server", "fixture"], env=env, capture_output=True, timeout=5)
        self.assertEqual(result.stdout, b'-c\nmodel_provider="openai"\napp-server\nfixture\n')

    def test_runtime_pins_own_endpoint_and_rejects_remote_or_missing_route(self):
        settings, args = self.install()
        runtime = args.install_root / "bin/codex-runtime"
        for port in [31001, 31002]:
            env = dict(os.environ, OPENAI_BASE_URL=f"http://127.0.0.1:{port}/v1")
            result = subprocess.run([str(runtime), "app-server"], env=env, capture_output=True, timeout=5)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(str(port).encode(), result.stdout)
        for endpoint in ["https://remote.invalid/v1", "http://127.0.0.1:8/v1?bad=1", ""]:
            result = subprocess.run([str(runtime), "app-server"], env=dict(os.environ, OPENAI_BASE_URL=endpoint), capture_output=True, timeout=5)
            self.assertNotEqual(result.returncode, 0)

    def test_listener_ownership_does_not_confuse_another_proxy(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            self.assertTrue(desktop.listener_owned(os.getpid(), listener.getsockname()[1]))
            self.assertFalse(desktop.listener_owned(1, listener.getsockname()[1]))

    def test_setup_refuses_recursive_native_and_unowned_wrapper(self):
        native = self.native()
        native.write_text('#!/bin/sh\n# AI_FENCE_CODEX_DESKTOP=1\n')
        with self.assertRaisesRegex(RuntimeError, "wrapper"):
            self.install(native)
        native = self.native()
        target = self.root / "install/bin/codex"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("unrelated user command")
        with self.assertRaisesRegex(RuntimeError, "unowned"):
            self.install(native)
        self.assertEqual(target.read_text(), "unrelated user command")

    def test_auth_seed_is_once_only_and_never_replaces_managed_refresh(self):
        seed = self.root / "seed"
        seed.mkdir()
        (seed / "auth.json").write_text("fixture-seed")
        self.settings["seed_home"] = str(seed)
        desktop.seed_auth(self.settings)
        self.assertEqual((self.home / "auth.json").read_text(), "fixture-seed")
        (self.home / "auth.json").write_text("fixture-managed-refresh")
        (seed / "auth.json").write_text("fixture-other-account")
        desktop.seed_auth(self.settings)
        self.assertEqual((self.home / "auth.json").read_text(), "fixture-managed-refresh")
        self.assertFalse((self.home / "auth.json").is_symlink())

    def test_previewed_isolated_workspace_reused_and_missing_workspace_needs_recovery(self):
        workspace = self.root / "workspaces/thread_isolated"
        workspace.mkdir(parents=True)
        (workspace / "dirty").write_text("keep uncommitted work")
        record = desktop.attach(self.settings, "thread_isolated", workspace, self.lease, mode="isolated")
        record["project"] = desktop.identity(self.project)
        record["leases"] = []
        desktop.save(desktop.record_path(self.settings, "thread_isolated"), record)
        self.settings["roots"] = [str(self.project)]
        args = SimpleNamespace(workspace_action="create", thread_id="thread_isolated", project=self.project,
                               directory=self.root / "workspaces", apply=True)
        with patch.object(desktop, "native_thread", return_value=dict(cwd=str(self.project))), contextlib.redirect_stdout(io.StringIO()):
            desktop.workspace_control(self.settings, args)
        self.assertEqual((workspace / "dirty").read_text(), "keep uncommitted work")
        (workspace / "dirty").unlink()
        workspace.rmdir()
        with patch.object(desktop, "native_thread", return_value=dict(cwd=str(self.project))), self.assertRaisesRegex(RuntimeError, "missing"):
            desktop.workspace_control(self.settings, args)

    def test_lifecycle_reserves_resume_lease_before_forwarding(self):
        record = desktop.attach(self.settings, "thread_reserve", self.project, self.lease)
        record["leases"] = []
        desktop.save(desktop.record_path(self.settings, "thread_reserve"), record)
        lifecycle = desktop.Lifecycle(self.settings, self.lease)
        request = dict(id=1, method="thread/resume", params=dict(threadId="thread_reserve"))
        lifecycle.request(request)
        record = desktop.load(desktop.record_path(self.settings, "thread_reserve"))
        self.assertEqual(record["leases"], [self.lease])
        self.assertEqual(request["params"]["cwd"], str(self.project))

    def test_setup_profile_identity_cannot_drift_and_rollback_preserves_history(self):
        settings, args = self.install()
        before = (args.install_root / "settings.json").read_bytes()
        args.profile = "another-account"
        with patch.dict(os.environ, {"AI_FENCE_DESKTOP_CLI_SOURCE":str(self.root / "fence-cli")}), self.assertRaisesRegex(RuntimeError, "identity"):
            desktop.install(args)
        self.assertEqual(before, (args.install_root / "settings.json").read_bytes())
        (self.root / "fence-cli").write_text("#!/bin/sh\n# upgraded\nexit 0\n")
        updated, args = self.install(self.root / "native-codex", self.root / "fence-cli")
        with contextlib.redirect_stdout(io.StringIO()):
            desktop.control(["--install-root", str(args.install_root), "rollback"])
        self.assertEqual(desktop.load(args.install_root / "settings.json")["release"], settings["release"])
        self.assertTrue((args.install_root / "releases" / updated["release"]).exists())

    def test_doctor_sanitizes_metadata_and_identifies_dead_explicit_endpoint(self):
        settings, args = self.install()
        (self.home / "auth.json").write_text("DO_NOT_OUTPUT_FIXTURE_AUTH")
        (self.home / "config.toml").write_text('model_provider="openai"\n[model_providers.custom]\napi_key="DO_NOT_OUTPUT_FIXTURE_CONFIG"\n')
        with socket.socket() as port:
            port.bind(("127.0.0.1", 0))
            unused = port.getsockname()[1]
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            healthy = desktop.doctor(settings, SimpleNamespace(endpoint=f"http://127.0.0.1:{unused}/v1", thread_id=None))
        self.assertFalse(healthy)
        self.assertNotIn("DO_NOT_OUTPUT", output.getvalue())
        report = json.loads(output.getvalue())
        self.assertFalse(report["endpoint_owner_verified"])
        self.assertEqual(report["state_home"], str(self.home))

    def test_uninstall_refuses_active_lease_and_changed_owned_commands(self):
        settings, args = self.install()
        desktop.save(args.install_root / "launches/fixture.json", dict(format=1, **self.lease))
        with self.assertRaisesRegex(RuntimeError, "active"):
            desktop.uninstall(settings, True)
        (args.install_root / "launches/fixture.json").unlink()
        (args.install_root / "bin/codex").write_text("edited user command")
        with self.assertRaisesRegex(RuntimeError, "changed"):
            desktop.uninstall(settings, True)
        self.assertTrue((args.install_root / "settings.json").exists())


    def test_failed_proxy_launcher_exits_cleanly_with_stdin_open(self):
        native = self.native()
        cli = self.root / "failing-fence"
        cli.write_text("#!/bin/sh\nexit 23\n")
        cli.chmod(0o700)
        settings, args = self.install(native, cli)
        child = subprocess.Popen([str(args.install_root / "bin/codex"), "app-server"],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertEqual(child.wait(timeout=5), 23)
            self.assertNotIn(b"Fatal Python error", child.stderr.read())
            self.assertFalse(list((args.install_root / "launches").glob("*.json")))
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
            child.stdin.close()
            child.stdout.close()
            child.stderr.close()

    def test_starting_launch_lease_protects_uninstall_before_receipt_registration(self):
        settings, args = self.install()
        with desktop.lock(args.install_root / ".lifecycle.lock", shared=True):
            with self.assertRaisesRegex(RuntimeError, "active or starting"):
                desktop.uninstall(settings, True)
        self.assertTrue((args.install_root / "bin/codex").exists())

    def test_runtime_cli_uses_supported_no_daemon_flag(self):
        native = self.native()
        native.write_text('#!/bin/sh\ncase "$1" in --version) echo "codex-cli 0.162.1";; --help) echo "--no-daemon";; *) printf "%s\\n" "$@";; esac\n')
        settings, args = self.install(native)
        self.assertTrue(settings["no_daemon_supported"])
        result = subprocess.run([str(args.install_root / "bin/codex-runtime"), "resume", "fixture"],
                                env=dict(os.environ, OPENAI_BASE_URL="http://127.0.0.1:31001/v1"), capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0)
        self.assertIn(b"--no-daemon", result.stdout)


if __name__ == "__main__":
    unittest.main()
