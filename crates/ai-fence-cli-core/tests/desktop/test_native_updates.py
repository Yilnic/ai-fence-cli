"""Actual CLI preparation/activation while an old native connection is live."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from http.server import BaseHTTPRequestHandler
from tls_fixture import start_tls

import test_desktop as fixtures
import test_native as native_fixture


@unittest.skipUnless(os.environ.get("AF_TEST_CLI") and os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_CLI and AF_TEST_NATIVE")
class NativeUpdates(unittest.TestCase):
    def test_real_cli_atomic_update_retains_live_endpoint_and_history(self):
        with tempfile.TemporaryDirectory(prefix="af-native-update-") as temporary:
            root = Path(temporary)
            state, home = root / "state", root / "home"
            state.mkdir()
            home.mkdir()
            auth = json.dumps({"auth_mode":"apikey", "OPENAI_API_KEY":"fixture-invalid-no-inference"})
            (state / "auth.json").write_text(auth)
            environment = {k:v for k,v in os.environ.items() if not k.startswith(("AI_FENCE_", "OPENAI_", "ANTHROPIC_", "CODEX_")) and not k.upper().endswith("_PROXY")}
            environment.update(HOME=str(home), XDG_CONFIG_HOME=str(root / "config"),
                               AI_FENCE_HOME=str(root / "fence"), AI_FENCE_SNAPSHOTS="off", OTEL_SDK_DISABLED="true")
            cli = Path(os.environ["AF_TEST_CLI"]).resolve()
            native = Path(os.environ["AF_TEST_NATIVE"]).resolve()
            install = root / "adapter"
            admission = root / "admission with spaces"
            admission_log = root / "admission.log"
            admission.write_text("#!/bin/sh\nprintf 'admitted\\n' >> '" + str(admission_log) + "'\n[ \"$1\" != -- ] || shift\nexec \"$@\"\n")
            admission.chmod(0o700)
            setup = [str(cli), "codex-desktop", "--install-root", str(install), "setup", "--native-codex", str(native),
                     "--state-home", str(state), "--auth-home", str(state), "--profile", "fixture", "--updates", "off", "--reconnect", "off",
                     "--launch-prefix", str(admission), "--"]
            installed = subprocess.run(setup, env=environment, capture_output=True, timeout=30)
            self.assertEqual(installed.returncode, 0, "isolated setup failed: " + installed.stderr.decode()[:900])
            original = json.loads((install / "settings.json").read_text())
            self.assertEqual(original["launch_prefix"], [str(admission), "--"])
            old_payload = fixtures.desktop.cli_payload(original)
            old_hash = hashlib.sha256(old_payload.read_bytes()).hexdigest()
            a = native_fixture.Client(install / "bin/codex", environment)
            b = None
            try:
                endpoint = a.endpoint(2)
                candidate = root / "new-cli"
                shutil.copyfile(cli, candidate)
                with candidate.open("ab") as stream:
                    stream.write(b"\nsynthetic-distinct-update-artifact\n")
                manifest = dict(version=original["cli_version"], target="linux-x86_64", filename="ai-fence-cli-linux-x86_64",
                    size_bytes=candidate.stat().st_size, sha256=hashlib.sha256(candidate.read_bytes()).hexdigest())
                requests = []
                class Handler(BaseHTTPRequestHandler):
                    def do_GET(self):
                        requests.append(self.path.split("?", 1)[0])
                        if "manifest" in self.path:
                            payload = json.dumps(manifest).encode()
                            self.send_response(200)
                            self.send_header("Content-Length", str(len(payload)))
                            self.end_headers()
                            self.wfile.write(payload)
                        else:
                            self.send_response(200)
                            self.send_header("Content-Length", str(candidate.stat().st_size))
                            self.end_headers()
                            with candidate.open("rb") as stream:
                                shutil.copyfileobj(stream, self.wfile)
                    def log_message(self, *args):
                        pass
                server, cert = start_tls(root, Handler)
                self.addCleanup(server.server_close)
                self.addCleanup(server.shutdown)
                manifest["download_url"] = server.origin + "/downloads/ai-fence-cli-linux-x86_64"
                environment["SSL_CERT_FILE"] = str(cert)
                settings = dict(original, cli_version="0.0.0", update_policy="auto", update_base_url=server.origin)
                (install / "settings.json").write_text(json.dumps(settings))
                # The connection itself must check, prepare, activate and exec
                # the new runtime before producing its initialization response.
                b = native_fixture.Client(install / "bin/codex", environment)
                result = json.loads((install / "update-status.json").read_text())
                self.assertEqual(result["status"], "installed", result)
                selected = json.loads((install / "settings.json").read_text())
                self.assertNotEqual(original["release"], selected["release"])
                self.assertEqual(selected["launch_prefix"], original["launch_prefix"])
                self.assertEqual(hashlib.sha256(old_payload.read_bytes()).hexdigest(), old_hash)
                self.assertEqual(a.endpoint(3), endpoint)
                self.assertNotEqual(b.endpoint(2), endpoint)
                self.assertEqual(a.endpoint(4), endpoint)
                report = subprocess.run([str(cli), "codex-desktop", "--install-root", str(install), "doctor"],
                                        env=environment, capture_output=True, check=True, timeout=15)
                self.assertTrue(json.loads(report.stdout)["running_older_release"])
                self.assertEqual(len([path for path in requests if "manifest" in path]), 1)
                self.assertTrue(any(path.endswith("ai-fence-cli-linux-x86_64") for path in requests))
            finally:
                if b:
                    b.close()
                a.close()
            self.assertEqual((state / "auth.json").read_text(), auth)
            self.assertFalse(list((install / "launches").glob("*.json")))
            self.assertGreaterEqual(len(admission_log.read_text().splitlines()), 2)
