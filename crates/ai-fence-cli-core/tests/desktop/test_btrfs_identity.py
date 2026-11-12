"""A WSL remount must retain history/auth/workspaces and registered updates."""
from pathlib import Path
import unittest
import uuid
from unittest.mock import patch

import test_desktop as fixtures
import test_desktop_updates as update_fixtures
import test_registered_updates as registry_fixtures
from test_shared_auth import bundle

desktop = fixtures.desktop
SUBVOLUME = str(uuid.UUID(int=1))
REAL_SUBVOLUME_UUID = desktop.auth_runtime.subvolume_uuid


class BtrfsIdentityTests(unittest.TestCase):
    native = fixtures.DesktopTests.native
    install = fixtures.DesktopTests.install

    def setUp(self):
        fixtures.DesktopTests.setUp(self)
        mock = patch.object(desktop.auth_runtime, "subvolume_uuid", return_value=SUBVOLUME)
        mock.start()
        self.addCleanup(mock.stop)

    def remounted(self, value):
        return dict(value, device=value["device"] + 1000)

    def test_ready_history_survives_remount_without_waiting(self):
        self.settings.update(native_codex=str(self.native()), readiness_seconds=0,
                             state_identity=self.remounted(desktop.identity(self.home)))
        with patch.object(desktop.time, "sleep", side_effect=AssertionError("must not wait")):
            desktop.wait_for_payloads(self.settings)

    def test_changed_path_inode_or_subvolume_and_plain_device_are_rejected(self):
        expected = desktop.identity(self.home)
        self.assertTrue(desktop.same_identity(expected, self.remounted(expected)))
        for field, value in [("physical", "/other"), ("inode", 123),
                             ("subvolume_uuid", "another-subvolume")]:
            with self.subTest(field=field):
                actual = dict(expected, **{field: value})
                self.assertFalse(desktop.same_identity(expected, actual))
                self.settings.update(native_codex=str(self.native()), readiness_seconds=0,
                                     state_identity=actual)
                with self.assertRaisesRegex(RuntimeError, "unavailable or changed"):
                    desktop.wait_for_payloads(self.settings)
        plain = dict(expected, subvolume_uuid=None)
        self.assertFalse(desktop.same_identity(plain, self.remounted(plain)))
        self.assertFalse(desktop.same_identity(expected, plain))

    def test_empty_or_invalid_subvolume_uuid_never_weakens_device_checks(self):
        for value in [None, str(uuid.UUID(int=0)), "invalid"]:
            with self.subTest(value=value):
                expected = dict(desktop.identity(self.home), subvolume_uuid=value)
                self.assertTrue(desktop.same_identity(expected, dict(expected)))
                self.assertFalse(desktop.same_identity(expected, self.remounted(expected)))

    def test_ioctl_empty_uuid_retains_legacy_identity_representation(self):
        with patch.object(desktop.auth_runtime.fcntl, "ioctl", return_value=0):
            self.assertEqual(REAL_SUBVOLUME_UUID(self.home), str(uuid.UUID(int=0)))

    def test_saved_workspace_resume_retains_alias_and_dirty_files_after_remount(self):
        alias = self.root / "alias"
        alias.symlink_to(self.project, target_is_directory=True)
        dirty = self.project / "untracked.txt"
        dirty.write_text("unsaved work")
        record = desktop.attach(self.settings, "thread_remount", alias, self.lease)
        record["workspace"] = self.remounted(record["workspace"])
        desktop.save(desktop.record_path(self.settings, "thread_remount"), record)
        parameters = dict(threadId="thread_remount")
        desktop.preflight_resume(self.settings, parameters)
        self.assertEqual(parameters["cwd"], str(alias))
        desktop.attach(self.settings, "thread_remount", self.project, self.lease)
        self.assertEqual(dirty.read_text(), "unsaved work")
        record["workspace"]["subvolume_uuid"] = "replacement"
        desktop.save(desktop.record_path(self.settings, "thread_remount"), record)
        with self.assertRaisesRegex(RuntimeError, "identity changed"):
            desktop.preflight_resume(self.settings, dict(threadId="thread_remount"))

    def test_setup_upgrades_existing_history_after_remount(self):
        (self.home / "history-marker").write_text("keep")
        settings, args = self.install()
        settings["state_identity"] = self.remounted(settings["state_identity"])
        settings["auth_identity"] = self.remounted(settings["auth_identity"])
        desktop.save(args.install_root / "settings.json", settings)
        updated, _ = self.install()
        self.assertEqual(updated["state_identity"], desktop.identity(self.home))
        self.assertEqual((self.home / "history-marker").read_text(), "keep")

    def test_native_auth_remount_preserves_account_binding(self):
        owner = self.root / "native-owner"
        owner.mkdir()
        (owner / "auth.json").write_text(bundle("fixture-fresh"))
        (owner / "auth.json").chmod(0o600)
        self.settings.update(desktop.auth_policy(self.home, owner),
                             state_identity=desktop.identity(self.home))
        self.settings["auth_identity"] = self.remounted(self.settings["auth_identity"])
        self.assertEqual(desktop.auth_runtime.access_tokens(self.settings)["accessToken"], "fixture-fresh")
        (owner / "auth.json").write_text(bundle("fixture-foreign", "other-account"))
        with self.assertRaisesRegex(RuntimeError, "account"):
            desktop.auth_runtime.access_tokens(self.settings)

    def test_legacy_auth_requires_saved_history_uuid_and_original_inode(self):
        self.settings.update(auth_home=str(self.home), state_identity=self.remounted(desktop.identity(self.home)))
        actual = desktop.auth_runtime.directory_identity(self.home)
        self.settings["auth_identity"] = self.remounted({k: v for k, v in actual.items() if k != "subvolume_uuid"})
        desktop.auth_runtime.validate_home(self.settings)
        for change in [{"inode": actual["inode"] + 1}, {"subvolume_uuid": "another-subvolume"}]:
            with self.subTest(change=change):
                self.assertFalse(desktop.auth_runtime.same_auth_identity(self.settings, dict(actual, **change)))
        for state in [None, dict(self.settings["state_identity"], inode=123),
                      dict(self.settings["state_identity"], subvolume_uuid="replacement")]:
            with self.subTest(state=state):
                settings = dict(self.settings, state_identity=state or {})
                with self.assertRaisesRegex(RuntimeError, "identity changed"):
                    desktop.auth_runtime.validate_home(settings)

    def test_legacy_auth_without_remount_can_add_uuid_on_separate_subvolume(self):
        actual = desktop.auth_runtime.directory_identity(self.home)
        settings = dict(auth_identity={k: v for k, v in actual.items() if k != "subvolume_uuid"})
        self.assertTrue(desktop.auth_runtime.same_auth_identity(settings, actual))
        self.assertFalse(desktop.auth_runtime.same_auth_identity(settings, self.remounted(actual)))

    def test_registered_terminal_update_is_not_skipped_after_remount(self):
        root = registry_fixtures.RegisteredUpdates.register(self)
        registry = desktop.installation_registry()
        record = desktop.load(registry)
        record["installations"][0]["identity"] = self.remounted(record["installations"][0]["identity"])
        desktop.save(registry, record)
        result, update, _ = registry_fixtures.RegisteredUpdates.check(self)
        self.assertEqual(result["installations"][0]["status"], "current")
        update.assert_called_once()
        record["installations"][0]["identity"]["subvolume_uuid"] = "replacement"
        desktop.save(registry, record)
        result, update, _ = registry_fixtures.RegisteredUpdates.check(self)
        self.assertEqual(result["installations"][0]["status"], "missing_or_replaced")
        update.assert_not_called()

    def test_update_activation_accepts_remount_metadata_and_legacy_auth_upgrade(self):
        _, fetch, run, _ = update_fixtures.UpdateTests.fixture(self)
        root = Path(self.settings["install_root"])
        self.settings.update(state_identity=self.remounted(desktop.identity(self.home)),
                             auth_identity=self.remounted({k: v for k, v in desktop.auth_runtime.directory_identity(self.home).items()
                                                           if k != "subvolume_uuid"}))
        desktop.save(root / "settings.json", self.settings)
        def prepare(argv, **kwargs):
            result = run(argv, **kwargs)
            if "setup" in argv:
                path = Path(argv[argv.index("--prepare-only") + 1]) / "settings.json"
                proposed = desktop.load(path)
                proposed.update(state_identity=desktop.identity(self.home),
                                auth_identity=desktop.auth_runtime.directory_identity(self.home))
                desktop.save(path, proposed)
            return result
        with patch.object(desktop.updates.auth_identity, "subvolume_uuid", return_value=SUBVOLUME):
            result = desktop.updates.check(self.settings, force=True, opener=fetch, run=prepare)
        self.assertEqual(result["status"], "installed", result)

    def test_update_activation_rejects_replaced_history_or_credentials(self):
        _, fetch, run, _ = update_fixtures.UpdateTests.fixture(self)
        root = Path(self.settings["install_root"])
        self.settings.update(state_identity=desktop.identity(self.home),
                             auth_identity=desktop.auth_runtime.directory_identity(self.home))
        desktop.save(root / "settings.json", self.settings)
        before = (root / "settings.json").read_bytes()
        for field in ["state_identity", "auth_identity"]:
            for change in [{"inode": 123}, {"subvolume_uuid": "replacement"}]:
                with self.subTest(field=field, change=change):
                    def prepare(argv, **kwargs):
                        result = run(argv, **kwargs)
                        if "setup" in argv:
                            path = Path(argv[argv.index("--prepare-only") + 1]) / "settings.json"
                            proposed = desktop.load(path)
                            proposed[field] = dict(proposed[field], **change)
                            desktop.save(path, proposed)
                        return result
                    result = desktop.updates.check(self.settings, force=True, opener=fetch, run=prepare)
                    self.assertEqual(result["status"], "failed", result)
                    self.assertEqual((root / "settings.json").read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
