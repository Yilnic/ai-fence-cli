"""Proxy supervision regressions; all state/processes are temporary or mocked."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

SOURCE = Path(__file__).resolve().parents[1] / "scripts/windows-proxy/proxy_service.py"
spec = importlib.util.spec_from_file_location("windows_proxy", SOURCE)
service = importlib.util.module_from_spec(spec)
with patch.dict(sys.modules, msvcrt=SimpleNamespace(locking=Mock(), LK_NBLCK=1)):
    spec.loader.exec_module(service)


class WindowsProxyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.binary = self.root / "proxy.exe"
        self.binary.write_bytes(b"fixture only")
        self.manifest = dict(filename="proxy.exe", sha256=hashlib.sha256(self.binary.read_bytes()).hexdigest())
        (self.root / "release-manifest.json").write_text(json.dumps(self.manifest))
        self.settings = dict(channel="prod", fence_url="https://fence.example.test", port=60441,
                             local_url="http://127.0.0.1:60441/v1")
        self.save_settings()
        (self.root / "api-key").write_text("fixture-key")
        flags = patch.object(service.subprocess, "CREATE_NO_WINDOW", 0, create=True)
        flags.start()
        self.addCleanup(flags.stop)

    def save_settings(self):
        (self.root / "proxy-settings.json").write_text(json.dumps(self.settings))

    def test_failed_child_restarts_and_stop_request_terminates_its_child(self):
        failed = Mock(pid=42, returncode=7)
        failed.poll.return_value = 7
        running = Mock(pid=43, returncode=0)
        running.poll.return_value = None

        def sleep(_):
            if launch.call_count == 2:
                (self.root / "stop-proxy").touch()

        with patch.object(service.subprocess, "Popen", side_effect=[failed, running]) as launch, \
                patch.object(service.time, "sleep", side_effect=sleep), \
                patch.dict(os.environ, AI_FENCE_DEV_API_KEY="wrong-channel-key"):
            service.supervise(self.root)
        self.assertEqual(launch.call_count, 2)
        running.terminate.assert_called_once()
        running.wait.assert_called_once_with(timeout=10)
        environment = launch.call_args.kwargs["env"]
        self.assertEqual(environment["AI_FENCE_CLI_CHANNEL"], "prod")
        self.assertEqual(environment["AI_FENCE_API_KEY"], "fixture-key")
        self.assertNotIn("AI_FENCE_DEV_API_KEY", environment)
        state = json.loads((self.root / "proxy-state.json").read_text())
        self.assertEqual(state["proxy_pid"], 43)
        self.assertEqual(state["binary_sha256"], self.manifest["sha256"])

    def test_tampered_release_is_rejected_before_process_start(self):
        self.binary.write_bytes(b"modified")
        with patch.object(service.subprocess, "Popen") as launch:
            with self.assertRaisesRegex(RuntimeError, "checksum"):
                service.supervise(self.root)
        launch.assert_not_called()

    def test_install_key_overrides_inherited_keys_in_both_channels(self):
        for channel in ("prod", "dev"):
            with self.subTest(channel=channel):
                (self.root / "stop-proxy").unlink(missing_ok=True)
                self.settings["channel"] = channel
                self.save_settings()
                child = Mock(pid=44, returncode=0)
                child.poll.return_value = None
                inherited = {name: "inherited-key" for name in
                             ("AI_FENCE_KEY", "AI_FENCE_API_KEY", "AI_FENCE_DEV_KEY", "AI_FENCE_DEV_API_KEY")}
                with patch.object(service.subprocess, "Popen", return_value=child) as launch, \
                        patch.object(service.time, "sleep", side_effect=lambda _: (self.root / "stop-proxy").touch()), \
                        patch.dict(os.environ, inherited):
                    service.supervise(self.root)
                environment = launch.call_args.kwargs["env"]
                key_name = "AI_FENCE_DEV_API_KEY" if channel == "dev" else "AI_FENCE_API_KEY"
                self.assertEqual(environment[key_name], "fixture-key")
                for name in inherited.keys() - {key_name}:
                    self.assertNotIn(name, environment)

    def test_legacy_settings_retain_dev_channel_and_stop_marker(self):
        self.settings.pop("channel")
        self.save_settings()
        (self.root / "stop-proxy").touch()
        with patch.object(service.subprocess, "Popen") as launch:
            service.supervise(self.root)
        launch.assert_not_called()

    def test_fatal_supervisor_error_returns_failure_to_task_scheduler(self):
        with patch.object(sys, "argv", ["proxy_service.py", "--root", str(self.root)]), \
                patch.object(service, "supervise", side_effect=RuntimeError("fixture failure")), \
                patch.object(service.logging, "basicConfig"), patch.object(service.logging, "exception") as log:
            with self.assertRaises(SystemExit) as error:
                service.main()
        self.assertEqual(error.exception.code, 1)
        log.assert_called_once()


if __name__ == "__main__":
    unittest.main()
