"""Native runtime upgrades use temporary ELF fixtures; no real credentials."""
import fcntl
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import sys
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop
updates = desktop.updates


def elf(marker):
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    header[18:20] = (62).to_bytes(2, "little")
    return bytes(header) + marker.encode()


class NativeUpdateTests(unittest.TestCase):
    def setUp(self):
        fixtures.DesktopTests.setUp(self)
        self.install = Path(self.settings["install_root"])
        self.install.mkdir()
        self.resources = self.root / "desktop-resources"
        self.resources.mkdir()
        self.old = self.root / "old-runtime"
        for name, relative in updates.NATIVE_FILES.items():
            (self.resources / name).write_bytes(elf("new-" + name))
            old = self.old / relative
            old.parent.mkdir(parents=True, exist_ok=True)
            old.write_bytes(elf("old-" + name))
            old.chmod(0o700)
        self.version = "codex-cli 0.165.0-alpha.1.2"
        self.settings.update(release="fixture", cli_version="0.1.13", cli_sha256="fixture-cli",
                             native_updates="desktop", native_codex=str(self.old / "bin/codex"),
                             native_version="codex-cli 0.164.0", native_sha256=desktop.digest(self.old / "bin/codex"),
                             auth_mode="native-owner", auth_home=str(self.home), readiness_seconds=0,
                             launch_prefix=[], update_policy="auto", channel="dev", update_base_url="https://ai-dev.yaaf.de",
                             files={})
        self.write_settings()
        self.before = (self.install / "settings.json").read_bytes()
        (self.home / "history-marker").write_text("keep")
        self.calls = []
        self.mutate_prepared = lambda value: None

    def write_settings(self):
        desktop.save(self.install / "settings.json", self.settings)

    def discover(self):
        return dict(resources=self.resources, version=self.version)

    def run_native(self, argv, **kwargs):
        self.calls.append(argv)
        self.assertEqual(kwargs.get("stdin"), subprocess.DEVNULL)
        if argv[-1] == "--version":
            return SimpleNamespace(returncode=0, stdout=(self.version + "\n").encode())
        self.assertIn("--prepare-only", argv)
        self.assertEqual(argv[argv.index("--native-updates") + 1], "desktop")
        native = Path(argv[argv.index("--native-codex") + 1])
        prepared = dict(self.settings, native_codex=str(native), native_version=self.version,
                        native_sha256=desktop.digest(native))
        self.mutate_prepared(prepared)
        desktop.save(Path(argv[argv.index("--prepare-only") + 1]) / "settings.json", prepared)
        return SimpleNamespace(returncode=0, stdout=b"")

    def check(self, **kwargs):
        with patch.object(updates, "require_bootstrap", return_value=dict(status="ok")):
            return updates.native_check(self.settings, fixtures.SOURCE, discover=self.discover, run=self.run_native, **kwargs)

    def assert_selection_unchanged(self):
        self.assertEqual((self.install / "settings.json").read_bytes(), self.before)
        self.assertFalse((self.install / "previous-settings.json").exists())

    def test_manual_selection_never_discovers_or_changes_runtime(self):
        self.settings["native_updates"] = "manual"
        with patch.object(updates, "desktop_runtime", side_effect=AssertionError("must not discover")):
            result = updates.native_check(self.settings, fixtures.SOURCE, discover=lambda: self.fail("must not discover"))
        self.assertEqual(result["status"], "disabled")
        self.assert_selection_unchanged()

    def test_verified_runtime_is_copied_independently_and_keeps_history_and_previous_binary(self):
        old = (self.old / "bin/codex").read_bytes()
        result = self.check()
        self.assertEqual(result["status"], "installed", result)
        selected = desktop.load(self.install / "settings.json")
        native = Path(selected["native_codex"])
        self.assertFalse(native.is_relative_to(self.resources))
        self.assertFalse(native.is_relative_to(self.install))
        self.assertEqual(selected["native_version"], self.version)
        self.assertEqual(desktop.load(self.install / "previous-settings.json"), self.settings)
        shutil.rmtree(self.resources)
        self.assertTrue(native.is_file())
        self.assertTrue((native.parent / "codex-code-mode-host").is_file())
        self.assertTrue((native.parent.parent / "codex-path/rg").is_file())
        self.assertEqual((self.old / "bin/codex").read_bytes(), old)
        self.assertEqual((self.home / "history-marker").read_text(), "keep")

    def test_matching_runtime_does_not_copy_probe_or_activate(self):
        for name, relative in updates.NATIVE_FILES.items():
            shutil.copyfile(self.resources / name, self.old / relative)
        self.settings["native_version"] = self.version
        self.settings["native_sha256"] = desktop.digest(self.old / "bin/codex")
        self.write_settings()
        result = self.check()
        self.assertEqual(result["status"], "current", result)
        self.assertFalse(self.calls)
        self.assertFalse(self.install.with_name(self.install.name + "-native").exists())

    def test_native_rollback_defers_for_connections_then_restores_and_pauses_updates(self):
        self.assertEqual(self.check()["status"], "installed")
        selected = (self.install / "settings.json").read_bytes()
        previous = (self.install / "previous-settings.json").read_bytes()
        with desktop.lock(self.install / ".lifecycle.lock", shared=True):
            with self.assertRaisesRegex(RuntimeError, "active or starting launch"):
                desktop.control(["--install-root", str(self.install), "rollback"])
        self.assertEqual((self.install / "settings.json").read_bytes(), selected)
        self.assertEqual((self.install / "previous-settings.json").read_bytes(), previous)
        with contextlib.redirect_stdout(io.StringIO()):
            desktop.control(["--install-root", str(self.install), "rollback"])
        restored = desktop.load(self.install / "settings.json")
        self.assertEqual(restored["native_codex"], self.settings["native_codex"])
        self.assertEqual(restored["native_updates"], "manual")
        self.assertEqual(restored["update_policy"], "manual")
        self.assertEqual((self.home / "history-marker").read_text(), "keep")

    def test_native_rollback_rejects_a_changed_previous_binary(self):
        self.assertEqual(self.check()["status"], "installed")
        selected = (self.install / "settings.json").read_bytes()
        (self.old / "bin/codex").write_bytes(elf("changed-old-native"))
        with self.assertRaisesRegex(RuntimeError, "previous native runtime integrity"):
            desktop.control(["--install-root", str(self.install), "rollback"])
        self.assertEqual((self.install / "settings.json").read_bytes(), selected)

    def test_companion_change_is_detected_even_when_codex_binary_and_version_match(self):
        shutil.copyfile(self.resources / "codex", self.old / "bin/codex")
        self.settings["native_version"] = self.version
        self.settings["native_sha256"] = desktop.digest(self.old / "bin/codex")
        self.write_settings()
        self.assertEqual(self.check()["status"], "installed")
        selected = desktop.load(self.install / "settings.json")
        self.assertNotEqual(Path(selected["native_codex"]).parent, self.old / "bin")

    def test_existing_history_connection_defers_upgrade_without_copying_or_restarting(self):
        with (self.install / ".lifecycle.lock").open("w") as connection:
            fcntl.flock(connection, fcntl.LOCK_SH)
            result = self.check()
        self.assertEqual(result["status"], "waiting_for_connections", result)
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()
        self.assertFalse(self.install.with_name(self.install.name + "-native").exists())

    def test_check_only_reports_available_without_activation(self):
        result = self.check(check_only=True)
        self.assertEqual(result["status"], "available")
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()

    def test_bootstrap_failure_keeps_selected_runtime_and_does_not_expose_raw_error(self):
        failure = updates.CandidateError(dict(status="failed", classification="protocol_rejected"))
        with patch.object(updates, "require_bootstrap", side_effect=failure):
            result = updates.native_check(self.settings, fixtures.SOURCE, discover=self.discover, run=self.run_native)
        self.assertEqual(result["failure_stage"], "bootstrap", result)
        self.assertEqual(result["classification"], "protocol_rejected")
        self.assert_selection_unchanged()

    def test_invalid_payload_is_not_executed(self):
        (self.resources / "codex").write_bytes(b"MZ" + b"invalid" * 10)
        result = self.check()
        self.assertEqual(result["failure_stage"], "payload", result)
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()

    def test_symlink_payload_is_not_executed(self):
        (self.resources / "codex").unlink()
        (self.resources / "codex").symlink_to(self.old / "bin/codex")
        self.assertEqual(self.check()["failure_stage"], "payload")
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()

    def test_older_desktop_version_is_not_an_automatic_downgrade(self):
        self.version = "codex-cli 0.163.0"
        self.assertEqual(self.check()["status"], "failed")
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()
        self.assertLess(updates.native_version_key("codex-cli 0.165.0-alpha.1.2"), updates.native_version_key("codex-cli 0.165.0"))
        self.assertLess(updates.native_version_key("codex-cli 0.165.0-alpha.1.2"), updates.native_version_key("codex-cli 0.165.0-alpha.1.10"))

    def test_prepared_authentication_or_admission_change_cannot_activate(self):
        for field, value in [("auth_home", "/other/account"), ("launch_prefix", ["/other/broker"]), ("native_updates", "manual")]:
            with self.subTest(field=field):
                self.mutate_prepared = lambda prepared, key=field, replacement=value: prepared.update({key: replacement})
                self.assertEqual(self.check()["status"], "failed")
                self.assert_selection_unchanged()

    def test_mismatched_version_is_not_activated(self):
        def run(argv, **kwargs):
            self.calls.append(argv)
            return SimpleNamespace(returncode=0, stdout=b"codex-cli 0.163.0\n")
        result = updates.native_check(self.settings, fixtures.SOURCE, discover=self.discover, run=run)
        self.assertEqual(result["failure_stage"], "version")
        self.assertEqual(len(self.calls), 1)
        self.assert_selection_unchanged()

    def test_cached_payload_tampering_is_not_executed(self):
        self.assertEqual(self.check()["status"], "installed")
        selected = desktop.load(self.install / "settings.json")
        Path(selected["native_codex"]).write_bytes(elf("tampered"))
        self.write_settings()
        (self.install / "previous-settings.json").unlink()
        self.calls.clear()
        result = self.check()
        self.assertEqual(result["failure_stage"], "copy", result)
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()

    def test_another_updater_does_not_block_startup(self):
        with (self.install / ".native-update.lock").open("w") as lease:
            fcntl.flock(lease, fcntl.LOCK_EX)
            result = self.check()
        self.assertEqual(result["status"], "busy")
        self.assertFalse(self.calls)
        self.assert_selection_unchanged()

    def test_discovery_failure_has_bounded_sanitized_diagnostics(self):
        def discover():
            raise OSError("fixture-secret-do-not-log")
        result = updates.native_check(self.settings, fixtures.SOURCE, discover=discover)
        self.assertEqual(result["failure_stage"], "discovery")
        self.assertNotIn("fixture-secret", json.dumps(result))
        self.assert_selection_unchanged()

    def test_running_store_app_discovery_is_translated_without_shell_interpolation(self):
        resource = r"C:\Program Files\WindowsApps\OpenAI.Codex_26.1023.6123.0_x64__2p2nqsd0c76g0\app\resources"
        calls = []
        def run(argv, **kwargs):
            calls.append((argv, kwargs))
            output = [rb"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                      json.dumps(dict(resources=resource, version=self.version)).encode(), str(self.resources).encode()]
            return SimpleNamespace(returncode=0, stdout=output[len(calls) - 1])
        with patch.dict(os.environ, {"WSL_INTEROP": "fixture"}), patch.object(updates.shutil, "which", return_value="/fixture/powershell.exe"):
            result = updates.desktop_runtime(run=run)
        self.assertEqual(result, self.discover())
        self.assertEqual(calls[2][0], ["/usr/bin/wslpath", "-u", "-a", resource])
        self.assertFalse(any(call[1].get("shell") for call in calls))

    def test_discovery_refuses_a_project_supplied_powershell_before_executing_it(self):
        calls = []
        def run(argv, **kwargs):
            calls.append(argv)
            return SimpleNamespace(returncode=0, stdout=rb"\\wsl.localhost\Ubuntu\tmp\powershell.exe")
        with patch.dict(os.environ, {"WSL_INTEROP": "fixture"}), patch.object(updates.shutil, "which", return_value="/tmp/powershell.exe"):
            with self.assertRaisesRegex(ValueError, "windows_desktop_unavailable"):
                updates.desktop_runtime(run=run)
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0][0], "/usr/bin/wslpath")

    def test_launch_reloads_a_concurrent_runtime_selection_after_taking_lifecycle_lock(self):
        changed = dict(self.settings, native_codex="/next/verified/codex", native_version=self.version)
        entered = []
        @contextlib.contextmanager
        def lifecycle(path, **kwargs):
            self.assertEqual(path, self.install / ".lifecycle.lock")
            self.assertTrue(kwargs.get("shared"))
            entered.append(True)
            desktop.save(self.install / "settings.json", changed)
            yield
        with patch.dict(os.environ, {"AI_FENCE_CODEX_DESKTOP": "1"}), \
                patch.object(sys, "argv", ["runtime.py", "adapter", "app-server"]), \
                patch.object(desktop, "settings_for_runtime", return_value=dict(self.settings)), \
                patch.object(desktop, "wait_for_payloads"), \
                patch.object(updates, "check", return_value=dict(status="current")), \
                patch.object(updates, "native_check", return_value=dict(status="busy")), \
                patch.object(desktop, "register_installation"), \
                patch.object(desktop, "reconnect_after_update", return_value=dict(status="none")), \
                patch.object(desktop, "lock", lifecycle), \
                patch.object(desktop, "adapter", side_effect=AssertionError("stale runtime must never start")), \
                patch.object(desktop.os, "execv", side_effect=RuntimeError("reexec-verified-selection")):
            with self.assertRaisesRegex(RuntimeError, "reexec-verified-selection"):
                desktop.main()
            self.assertEqual(json.loads(os.environ["_AI_FENCE_DESKTOP_SETTINGS"]), changed)
        self.assertTrue(entered)

    def test_validation_failure_reports_warning_and_retains_previous_launch_selection(self):
        with patch.dict(os.environ, {"AI_FENCE_CODEX_DESKTOP": "1"}), \
                patch.object(sys, "argv", ["runtime.py", "adapter", "app-server"]), \
                patch.object(desktop, "settings_for_runtime", return_value=dict(self.settings)), \
                patch.object(desktop, "wait_for_payloads"), \
                patch.object(updates, "check", return_value=dict(status="current")), \
                patch.object(updates, "native_check", return_value=dict(status="failed")), \
                patch.object(desktop, "register_installation"), \
                patch.object(desktop, "reconnect_after_update", return_value=dict(status="none")), \
                patch.object(desktop, "adapter", return_value=7) as adapter, \
                contextlib.redirect_stderr(io.StringIO()) as diagnostics:
            self.assertEqual(desktop.main(), 7)
        self.assertEqual(adapter.call_args.args[0], self.settings)
        self.assertIn("retaining the selected runtime", diagnostics.getvalue())


if __name__ == "__main__":
    unittest.main()
