"""Terminal auto-update convergence for explicitly registered desktop installs."""
import contextlib
import io
import json
import os
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop


class RegisteredUpdates(unittest.TestCase):
    setUp = fixtures.DesktopTests.setUp

    def register(self):
        self.settings.update(release="old", cli_version="0.1.8", update_policy="auto", update_base_url="https://fence.example.test")
        root = Path(self.settings["install_root"])
        root.mkdir()
        desktop.save(root / "settings.json", self.settings)
        desktop.register_installation(self.settings)
        return root

    def check(self, **overrides):
        with patch.dict(os.environ, {"AI_FENCE_DESKTOP_CLI_SOURCE": "/fixture/cli"}), patch.object(desktop, "trusted", return_value=Path("/fixture/cli")), patch.object(desktop.subprocess, "run", return_value=SimpleNamespace(returncode=0, stdout=b"ai-fence-cli 0.1.9\n")), patch.object(desktop.updates, "check", return_value={"status": "current"}) as update, patch.object(desktop, "reconnect", return_value={"status": "scheduled"}) as reconnect, contextlib.redirect_stderr(io.StringIO()):
            result = desktop.update_registered("https://fence.example.test")
            return result, update, reconnect

    def test_registration_is_idempotent_and_channel_scoped(self):
        root = self.register()
        path = desktop.installation_registry()
        before = path.read_bytes()
        desktop.register_installation(self.settings)
        self.assertEqual(path.read_bytes(), before)
        with patch.dict(os.environ, {"AI_FENCE_CLI_CHANNEL": "dev"}):
            self.assertNotEqual(desktop.installation_registry(), path)
            result, update, reconnect = self.check()
            self.assertEqual(result["status"], "none")
            update.assert_not_called()
            reconnect.assert_not_called()

    def test_unregister_retains_other_installations(self):
        root = self.register()
        registry = desktop.installation_registry()
        record = desktop.load(registry)
        record["installations"].append(dict(record["installations"][0], install_root=str(self.root / "another-install")))
        desktop.save(registry, record)
        desktop.unregister_installation(self.settings)
        self.assertEqual([v["install_root"] for v in desktop.load(registry)["installations"]], [str(self.root / "another-install")])

    def test_new_terminal_binary_bypasses_daily_deferral_and_reconnects(self):
        self.register()
        result, update, reconnect = self.check()
        self.assertEqual(result["status"], "checked")
        self.assertTrue(update.call_args.kwargs["force"])
        self.assertTrue(reconnect.call_args.kwargs["automatic"])
        self.assertTrue(reconnect.call_args.kwargs["older_only"])

    def test_environment_and_saved_policy_disable_reconnect_but_allow_updates(self):
        root = self.register()
        with patch.dict(os.environ, {"AI_FENCE_CODEX_RECONNECT": "off"}):
            result, update, reconnect = self.check()
            update.assert_called_once()
            reconnect.assert_not_called()
        desktop.save(root / "settings.json", dict(self.settings, reconnect_policy="off"))
        result, update, reconnect = self.check()
        update.assert_called_once()
        reconnect.assert_not_called()

    def test_manual_source_mismatch_missing_and_replaced_homes_are_skipped(self):
        root = self.register()
        for change in [{"update_policy": "manual"}, {"update_base_url": "https://other.example.test"}, {"profile": "other-account"}]:
            desktop.save(root / "settings.json", dict(self.settings, **change))
            result, update, reconnect = self.check()
            update.assert_not_called()
            reconnect.assert_not_called()
        root.rename(root.with_name("saved-install"))
        result, update, reconnect = self.check()
        self.assertEqual(result["installations"][0]["status"], "missing_or_replaced")
        update.assert_not_called()
        self.assertFalse(root.exists())

    def test_failed_busy_and_unverified_deferred_checks_never_reconnect(self):
        root = self.register()
        for status in ["failed", "busy", "deferred", "unsupported_target"]:
            with patch.object(desktop.updates, "check", return_value={"status": status}), patch.object(desktop, "reconnect") as reconnect:
                self.assertEqual(desktop.reconnect_after_update(self.settings, {"status": status})["status"], "not_scheduled")
                reconnect.assert_not_called()
        (root / "update-status.json").write_text(json.dumps({"status": "current"}))
        with patch.object(desktop, "reconnect", return_value={"status": "scheduled"}) as reconnect:
            self.assertEqual(desktop.reconnect_after_update(self.settings, {"status": "deferred"})["status"], "scheduled")
            reconnect.assert_called_once()


if __name__ == "__main__":
    unittest.main()
