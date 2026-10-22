"""Desktop channels select code/configuration, never a different history account."""
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


class ChannelTests(unittest.TestCase):
    setUp = fixtures.DesktopTests.setUp
    native = fixtures.DesktopTests.native
    install = fixtures.DesktopTests.install

    def configure_dev(self):
        directory = self.root / "config/ai-fence-dev"
        directory.mkdir(parents=True)
        (directory / "cli.toml").write_text('fence_url="https://dev.example.test"\n')

    def test_dev_defaults_use_channel_root_and_default_profile(self):
        with patch.dict(os.environ, {"AI_FENCE_CLI_CHANNEL": "dev", "AI_FENCE_PROFILE": "default", "AI_FENCE_CODEX_PROFILE": "default"}), \
                patch.object(desktop, "install") as install:
            desktop.control(["setup", "--native-codex", "/fixture/native", "--state-home", "/fixture/state"])
        args = install.call_args.args[0]
        self.assertEqual(args.install_root.name, "ai-fence-codex-desktop-dev")
        self.assertEqual(args.profile, "default")

    def test_dev_setup_preserves_history_auth_and_explicit_setup_can_switch_one_root(self):
        settings, args = self.install()
        (self.home / "history-marker").write_text("keep history")
        (self.home / "auth.json").write_text("keep auth")
        self.configure_dev()
        args.channel = "dev"
        cli = self.root / "fence-cli"
        with patch.dict(os.environ, {"AI_FENCE_CLI_CHANNEL": "prod", "AI_FENCE_URL": "https://production.invalid"}), \
                patch.dict(os.environ, {"AI_FENCE_DESKTOP_CLI_SOURCE": str(cli)}), contextlib.redirect_stdout(io.StringIO()):
            desktop.install(args)
            before = (args.install_root / "settings.json").read_bytes()
            desktop.install(args)
        changed = desktop.load(args.install_root / "settings.json")
        self.assertEqual(changed["channel"], "dev")
        self.assertEqual(changed["update_base_url"], "https://dev.example.test")
        self.assertEqual(changed["profile"], settings["profile"])
        self.assertEqual(changed["state_identity"], settings["state_identity"])
        self.assertEqual(changed["auth_identity"], settings["auth_identity"])
        self.assertEqual((args.install_root / "settings.json").read_bytes(), before)
        self.assertEqual((self.home / "history-marker").read_text(), "keep history")
        self.assertEqual((self.home / "auth.json").read_text(), "keep auth")

    def test_official_channel_defaults_and_saved_source_precedence(self):
        args = SimpleNamespace(channel="dev", update_base_url=None)
        with patch.dict(os.environ, {"AI_FENCE_URL": "https://production.invalid", "AI_FENCE_DEV_URL": ""}):
            self.assertEqual(desktop.setup_channel_options(args, None), ("dev", "https://ai-dev.yaaf.de"))
            self.configure_dev()
            self.assertEqual(desktop.setup_channel_options(args, None), ("dev", "https://dev.example.test"))
            self.assertEqual(desktop.setup_channel_options(args, {"channel":"dev", "update_base_url":"https://pinned.example.test"}),
                             ("dev", "https://pinned.example.test"))

    def test_production_default_and_unknown_channel(self):
        with patch.dict(os.environ, {"AI_FENCE_URL": "https://unrelated.invalid"}):
            self.assertEqual(desktop.setup_channel_options(SimpleNamespace(channel="prod", update_base_url=None), None),
                             ("prod", "https://ai.yaaf.de"))
            with self.assertRaisesRegex(RuntimeError, "configure this channel"):
                desktop.setup_channel_options(SimpleNamespace(channel="staging", update_base_url=None), None)

    def test_legacy_explicit_source_is_not_silently_migrated(self):
        for selected in ["prod", "dev"]:
            args = SimpleNamespace(channel=selected, update_base_url=None)
            old = {"channel": selected, "update_base_url": "https://ai.yml.world"}
            self.assertEqual(desktop.setup_channel_options(args, old), (selected, "https://ai.yml.world"))

    def test_runtime_and_registration_bind_saved_channel_despite_opposite_environment(self):
        settings = dict(self.settings, channel="dev")
        root = Path(settings["install_root"])
        root.mkdir()
        desktop.save(root / "settings.json", settings)
        with patch.dict(os.environ, {"AI_FENCE_CLI_CHANNEL": "prod"}):
            desktop.register_installation(settings)
            self.assertTrue(desktop.installation_registry("dev").is_file())
            self.assertFalse(desktop.installation_registry("prod").exists())
            environment = dict(os.environ)
            desktop.bind_channel(settings, environment)
        self.assertEqual(environment["AI_FENCE_CLI_CHANNEL"], "dev")
        desktop.bind_channel(self.settings, environment)
        self.assertEqual(environment["AI_FENCE_CLI_CHANNEL"], "prod")

    def test_invalid_channels_are_rejected(self):
        for value in ["../prod", "dev/../../prod", "dev with spaces", 0, False, {}, []]:
            with self.assertRaises(ValueError):
                desktop.updates.channel(value)

    def test_single_command_switch_reuses_exact_history_auth_and_admission(self):
        settings, args = self.install()
        settings["launch_prefix"] = ["/fixture/existing admission", "--"]
        desktop.save(args.install_root / "settings.json", settings)
        self.configure_dev()
        with patch.dict(os.environ, {"AI_FENCE_CLI_CHANNEL":"dev"}), patch.object(desktop, "install") as install:
            desktop.control(["--install-root", str(args.install_root), "switch-channel"])
        selected = install.call_args.args[0]
        self.assertEqual(selected.channel, "dev")
        self.assertEqual(selected.update_base_url, "https://dev.example.test")
        self.assertEqual(str(selected.state_home), settings["state_home"])
        self.assertEqual(str(selected.auth_home), settings["auth_home"])
        self.assertEqual(selected.profile, settings["profile"])
        self.assertEqual(selected.launch_prefix, settings["launch_prefix"])
        self.assertEqual(selected.snapshots, settings["snapshot_policy"])

    def test_switch_refuses_to_guess_an_existing_adapter(self):
        with patch.dict(os.environ, {"AI_FENCE_CLI_CHANNEL":"dev"}), contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                desktop.control(["switch-channel"])


if __name__ == "__main__":
    unittest.main()
