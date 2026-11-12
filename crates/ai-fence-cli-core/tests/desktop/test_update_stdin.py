"""Launch-time helpers must not steal the desktop's early initialize frame."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import test_desktop as fixtures


FRAME = dict(id=1, method="initialize", params=dict(
    clientInfo=dict(name="fixture_desktop", version="1")))
RESOURCE = r"C:\Program Files\WindowsApps\OpenAI.Codex_26.1023.6123.0_x64__2p2nqsd0c76g0\app\resources"


def discovery_worker(fail_at):
    """A separate parent pipe lets real helper children attempt to drain fd 0."""
    desktop = fixtures.desktop
    updates = desktop.updates
    calls = []

    def helper(argv, **kwargs):
        calls.append(argv)
        outputs = [r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                   json.dumps(dict(resources=RESOURCE, version="codex-cli 0.165.0-alpha.1.2")),
                   "/fixture/desktop/resources"]
        # With inherited stdin this child consumes the queued initialize frame.
        # With DEVNULL it sees EOF while the owning adapter's pipe stays intact.
        program = ("import sys; sys.stdin.buffer.read(); "
                   "sys.stdout.write(sys.argv[1]); sys.exit(int(sys.argv[2]))")
        return subprocess.run([sys.executable, "-c", program, outputs[len(calls) - 1],
                               "7" if len(calls) == fail_at else "0"], **kwargs)

    def native_check(settings, script):
        assert settings["native_updates"] == "desktop"
        try:
            updates.desktop_runtime(run=helper)
            return dict(status="current")
        except ValueError:
            return dict(status="failed")

    def adapter(settings, arguments):
        line = sys.stdin.buffer.readline()
        request = json.loads(line) if line else None
        print(json.dumps(dict(request=request, helper_count=len(calls),
                              native_updates=settings["native_updates"])))
        return 0

    with tempfile.TemporaryDirectory(prefix="af-stdin-test-") as temporary:
        root = Path(temporary)
        settings = dict(format=1, install_root=str(root), release="fixture",
                        native_updates="desktop", channel="dev", update_base_url="https://ai-dev.yaaf.de")
        (root / "settings.json").write_text(json.dumps(settings))
        with patch.dict(os.environ, {"AI_FENCE_CODEX_DESKTOP": "1", "WSL_INTEROP": "fixture"}), \
                patch.object(sys, "argv", ["runtime.py", "adapter", "app-server"]), \
                patch.object(desktop, "settings_for_runtime", return_value=settings), \
                patch.object(desktop, "wait_for_payloads"), \
                patch.object(updates, "check", return_value=dict(status="current")), \
                patch.object(desktop, "register_installation"), \
                patch.object(desktop, "reconnect_after_update", return_value=dict(status="none")), \
                patch.object(updates, "native_check", side_effect=native_check), \
                patch.object(updates.shutil, "which", return_value="/fixture/powershell.exe"), \
                patch.object(desktop, "adapter", side_effect=adapter):
            return desktop.main()


class UpdateStdinTests(unittest.TestCase):
    def startup(self, fail_at=0):
        child = subprocess.run([sys.executable, "-B", str(Path(__file__).resolve()),
                                "--discovery-worker", str(fail_at)],
                               input=(json.dumps(FRAME) + "\n").encode(),
                               capture_output=True, timeout=15)
        self.assertEqual(child.returncode, 0, child.stderr.decode(errors="replace"))
        result = json.loads(child.stdout)
        self.assertEqual(result["request"], FRAME,
                         "desktop discovery consumed the adapter's initialize frame")
        self.assertEqual(result["native_updates"], "desktop")
        return result

    def test_immediate_initialize_survives_launch_time_desktop_discovery(self):
        self.assertEqual(self.startup()["helper_count"], 3)

    def test_discovery_failure_preserves_initialize_for_retained_runtime(self):
        for stage in [1, 2, 3]:
            with self.subTest(failing_helper=stage):
                self.assertEqual(self.startup(fail_at=stage)["helper_count"], stage)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--discovery-worker":
        raise SystemExit(discovery_worker(int(sys.argv[2])))
    unittest.main()
