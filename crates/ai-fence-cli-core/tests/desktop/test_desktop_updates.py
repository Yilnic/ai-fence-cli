"""Checksums, bounded launch updates and last-known working releases."""
import hashlib
import io
import json
import platform
from pathlib import Path
from types import SimpleNamespace
import unittest
import fcntl
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop


class UpdateTests(unittest.TestCase):
    setUp = fixtures.DesktopTests.setUp

    def fixture(self, tamper=False):
        binary = b"fixture-new-cli"
        self.settings.update(cli_version="0.1.8", release="old", update_policy="auto",
                             update_base_url="https://ai.yml.world", update_interval_seconds=86400,
                             native_codex="/fixture/native", auth_home=str(self.home))
        root = Path(self.settings["install_root"])
        root.mkdir()
        (root / "settings.json").write_text(json.dumps(self.settings))
        manifest = dict(version="0.1.9", target="linux-x86_64", filename="ai-fence-cli-linux-x86_64",
            sha256=hashlib.sha256(binary).hexdigest(), size_bytes=len(binary),
            download_url="https://github.com/Yilnic/ai-fence-cli/releases/download/v0.1.9/ai-fence-cli-linux-x86_64")
        calls = []
        def fetch(request, timeout):
            return io.BytesIO(json.dumps(manifest).encode() if "manifest" in request.full_url else binary + (b"X" if tamper else b""))
        def run(argv, **kwargs):
            calls.append(argv)
            if "setup" in argv:
                prepared = dict(self.settings, cli_version="0.1.9", cli_sha256=manifest["sha256"], release="new")
                (Path(argv[argv.index("--prepare-only") + 1]) / "settings.json").write_text(json.dumps(prepared))
            return SimpleNamespace(returncode=0, stdout=b"ai-fence-cli 0.1.9\n")
        return manifest, fetch, run, calls

    def test_verified_update_activates_via_official_setup_without_touching_history(self):
        manifest, fetch, run, calls = self.fixture()
        self.settings.update(native_codex="/fixture/native", state_home=str(self.home), auth_home=str(self.home))
        (Path(self.settings["install_root"]) / "settings.json").write_text(json.dumps(self.settings))
        (self.home / "history-marker").write_text("keep")
        result = desktop.updates.check(self.settings, force=True, opener=fetch, run=run)
        self.assertEqual(result["status"], "installed")
        self.assertEqual(calls[0][1:], ["--version"])
        self.assertIn("setup", calls[1])
        self.assertIn(str(self.home), calls[1])
        self.assertEqual((self.home / "history-marker").read_text(), "keep")

    def test_tampered_download_never_executes_or_changes_selected_release(self):
        manifest, fetch, run, calls = self.fixture(tamper=True)
        result = desktop.updates.check(self.settings, force=True, opener=fetch, run=run)
        self.assertEqual(result["status"], "failed")
        self.assertFalse(calls)
        self.assertEqual(json.loads((Path(self.settings["install_root"]) / "settings.json").read_text())["release"], "old")

    def test_failed_check_is_bounded_and_uses_backoff_without_replacing_payload(self):
        manifest, fetch, run, calls = self.fixture()
        def unavailable(request, timeout):
            raise OSError("fixture unavailable")
        self.assertEqual(desktop.updates.check(self.settings, force=True, opener=unavailable, run=run)["status"], "failed")
        self.assertEqual(desktop.updates.check(self.settings, opener=unavailable, run=run)["status"], "deferred")
        self.assertFalse(calls)

    def test_check_only_and_downgrade_do_not_activate(self):
        manifest, fetch, run, calls = self.fixture()
        result = desktop.updates.check(self.settings, force=True, check_only=True, opener=fetch, run=run)
        self.assertEqual(result["status"], "available")
        self.assertFalse(calls)
        self.settings["cli_version"] = "0.2.0"
        result = desktop.updates.check(self.settings, force=True, opener=fetch, run=run)
        self.assertEqual(result["status"], "current")
        self.assertFalse(calls)

    def test_concurrent_launch_has_one_updater_and_does_not_wait_on_it(self):
        manifest, fetch, run, calls = self.fixture()
        with (Path(self.settings["install_root"]) / ".update.lock").open("w") as lease:
            fcntl.flock(lease, fcntl.LOCK_EX)
            self.assertEqual(desktop.updates.check(self.settings, force=True, opener=fetch, run=run)["status"], "busy")
        self.assertFalse(calls)

    def test_interrupted_update_leaves_backoff_for_the_next_connection(self):
        manifest, fetch, run, calls = self.fixture()
        def interrupted(request, timeout):
            raise KeyboardInterrupt()
        with self.assertRaises(KeyboardInterrupt):
            desktop.updates.check(self.settings, force=True, opener=interrupted, run=run)
        self.assertEqual(desktop.updates.check(self.settings, opener=interrupted, run=run)["status"], "deferred")
        self.assertFalse(calls)

    def test_prepared_profile_change_and_failed_install_leave_settings_byte_identical(self):
        manifest, fetch, run, calls = self.fixture()
        settings_path = Path(self.settings["install_root"]) / "settings.json"
        before = settings_path.read_bytes()
        def wrong_profile(argv, **kwargs):
            result = run(argv, **kwargs)
            if "--prepare-only" in argv:
                prepared = Path(argv[argv.index("--prepare-only") + 1]) / "settings.json"
                data = json.loads(prepared.read_text())
                data["profile"] = "other-account"
                prepared.write_text(json.dumps(data))
            return result
        result = desktop.updates.check(self.settings, force=True, opener=fetch, run=wrong_profile)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(settings_path.read_bytes(), before)
        self.assertFalse((Path(self.settings["install_root"]) / "previous-settings.json").exists())

    def test_concurrent_policy_change_with_same_release_is_not_undone(self):
        manifest, fetch, run, calls = self.fixture()
        settings_path = Path(self.settings["install_root"]) / "settings.json"
        changed = dict(self.settings, update_base_url="https://reviewed.example", update_interval_seconds=3600)
        def concurrent_setup(argv, **kwargs):
            result = run(argv, **kwargs)
            if "--prepare-only" in argv:
                settings_path.write_text(json.dumps(changed))
            return result
        result = desktop.updates.check(self.settings, force=True, opener=fetch, run=concurrent_setup)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["failure_stage"], "activation")
        self.assertEqual(json.loads(settings_path.read_text()), changed)
        self.assertFalse((settings_path.parent / "previous-settings.json").exists())

    def test_unsupported_architecture_does_not_download_or_execute_x86_binary(self):
        manifest, fetch, run, calls = self.fixture()
        with patch.object(platform, "machine", return_value="aarch64"):
            result = desktop.updates.check(self.settings, force=True,
                opener=lambda *a, **kw: self.fail("must not download an incompatible target"), run=run)
        self.assertEqual(result["status"], "unsupported_target")
        self.assertFalse(calls)

    def dev_fixture(self):
        manifest, fetch, run, calls = self.fixture()
        self.settings.update(channel="dev", cli_version="0.1.9", cli_sha256="0" * 64,
                             update_base_url="https://dev.example.test")
        manifest.update(cli_name="ai-fence-cli-dev", download_url="https://dev.example.test/downloads/ai-fence-cli-linux-x86_64")
        (Path(self.settings["install_root"]) / "settings.json").write_text(json.dumps(self.settings))
        return manifest, fetch, run, calls

    def test_dev_same_version_changed_hash_installs_and_keeps_channel(self):
        manifest, fetch, run, calls = self.dev_fixture()
        result = desktop.updates.check(self.settings, force=True, opener=fetch, run=run)
        self.assertEqual(result["status"], "installed")
        self.assertIn("--channel", calls[1])
        self.assertEqual(calls[1][calls[1].index("--channel") + 1], "dev")
        observed = json.loads((Path(self.settings["install_root"]) / "settings.json").read_text())
        self.assertEqual(observed["channel"], "dev")
        self.assertEqual(observed["update_base_url"], self.settings["update_base_url"])

    def test_dev_same_hash_is_current_and_changed_hash_is_available_in_check_only(self):
        manifest, fetch, run, calls = self.dev_fixture()
        self.assertEqual(desktop.updates.check(self.settings, force=True, check_only=True, opener=fetch, run=run)["status"], "available")
        self.settings["cli_sha256"] = manifest["sha256"]
        self.assertEqual(desktop.updates.check(self.settings, force=True, opener=fetch, run=run)["status"], "current")
        self.assertFalse(calls)

    def test_prod_same_version_never_replaces_immutable_release_by_hash(self):
        manifest, fetch, run, calls = self.fixture()
        self.settings["cli_version"] = manifest["version"]
        self.settings["cli_sha256"] = "0" * 64
        self.assertEqual(desktop.updates.check(self.settings, force=True, opener=fetch, run=run)["status"], "current")
        self.assertFalse(calls)

    def test_dev_rejects_prod_manifest_or_foreign_download_before_execution(self):
        manifest, fetch, run, calls = self.dev_fixture()
        for changed in [{"cli_name":"ai-fence-cli"}, {"download_url":"https://other.example.test/cli"}]:
            before = dict(manifest)
            manifest.update(changed)
            self.assertEqual(desktop.updates.check(self.settings, force=True, opener=fetch, run=run)["status"], "failed")
            manifest.clear()
            manifest.update(before)
        self.assertFalse(calls)

    def test_automatic_activation_cannot_rebind_channel_or_update_source(self):
        manifest, fetch, run, calls = self.dev_fixture()
        path = Path(self.settings["install_root"]) / "settings.json"
        before = path.read_bytes()
        for changed in [{"channel":"prod"}, {"update_base_url":"https://other.example.test"}]:
            def prepare(argv, **kwargs):
                result = run(argv, **kwargs)
                if "setup" in argv:
                    staged = Path(argv[argv.index("--prepare-only") + 1]) / "settings.json"
                    settings = json.loads(staged.read_text())
                    settings.update(changed)
                    staged.write_text(json.dumps(settings))
                return result
            result = desktop.updates.check(self.settings, force=True, opener=fetch, run=prepare)
            self.assertEqual(result["status"], "failed")
            self.assertEqual(result["failure_stage"], "activation")
            self.assertEqual(path.read_bytes(), before)
