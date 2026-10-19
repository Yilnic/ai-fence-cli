"""Actual terminal auto-update -> registered desktop release -> idle reconnect."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from http.server import BaseHTTPRequestHandler

from test_native import Client
from tls_fixture import start_tls


@unittest.skipUnless(os.environ.get("AF_TEST_CLI") and os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_CLI and AF_TEST_NATIVE")
class NativeRegisteredUpdates(unittest.TestCase):
    def test_terminal_auto_update_reconnects_by_default_and_respects_both_opt_outs(self):
        with tempfile.TemporaryDirectory(prefix="af-terminal-update-") as temporary:
            root = Path(temporary)
            cli, native = Path(os.environ["AF_TEST_CLI"]).resolve(), Path(os.environ["AF_TEST_NATIVE"]).resolve()
            candidate = root / "candidate-cli"
            shutil.copyfile(cli, candidate)
            with candidate.open("ab") as stream:
                stream.write(b"\nsynthetic-terminal-update-artifact\n")
            candidate.chmod(0o700)
            version = subprocess.check_output([str(cli), "--version"], text=True).strip().removeprefix("ai-fence-cli ")
            with candidate.open("rb") as stream:
                candidate_hash = hashlib.file_digest(stream, "sha256").hexdigest()
            manifest = dict(version=version, target="linux-x86_64", filename="ai-fence-cli-linux-x86_64",
                            sha256=candidate_hash, size_bytes=candidate.stat().st_size)
            class Handler(BaseHTTPRequestHandler):
                def do_GET(self):
                    payload = json.dumps(manifest).encode() if "manifest" in self.path else None
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(payload) if payload else candidate.stat().st_size))
                    self.end_headers()
                    if payload:
                        self.wfile.write(payload)
                    else:
                        with candidate.open("rb") as stream:
                            shutil.copyfileobj(stream, self.wfile)
                def log_message(self, *args):
                    pass
            server, cert = start_tls(root, Handler)
            self.addCleanup(server.server_close)
            self.addCleanup(server.shutdown)
            manifest["download_url"] = server.origin + "/downloads/ai-fence-cli-linux-x86_64"
            environment = {k: v for k, v in os.environ.items() if not k.startswith(("AI_FENCE_", "CODEX_", "OPENAI_", "ANTHROPIC_")) and not k.upper().endswith("_PROXY")}
            home = root / "home"
            home.mkdir()
            environment.update(HOME=str(home), XDG_CONFIG_HOME=str(root / "config"), AI_FENCE_HOME=str(root / "fence"),
                               AI_FENCE_CLI_AUTO_UPDATE="never", AI_FENCE_SNAPSHOTS="off", OTEL_SDK_DISABLED="true",
                               SSL_CERT_FILE=str(cert), CURL_CA_BUNDLE=str(cert))
            clients, installs, states = [], [], []
            try:
                for name, policy in [("default", "auto"), ("disabled", "off")]:
                    state, install = root / (name + "-state"), root / (name + "-install")
                    state.mkdir()
                    (state / "auth.json").write_text('{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-invalid-no-inference"}')
                    setup = subprocess.run([str(cli), "codex-desktop", "--install-root", str(install), "setup",
                        "--native-codex", str(native), "--state-home", str(state), "--auth-home", str(state),
                        "--profile", name, "--updates", "off", "--reconnect", policy, "--update-base-url", server.origin],
                        env=environment, capture_output=True, timeout=30)
                    self.assertEqual(setup.returncode, 0, "temporary desktop setup failed")
                    clients.append(Client(install / "bin/codex", environment))
                    settings = json.loads((install / "settings.json").read_text())
                    settings.update(cli_version="0.0.0", update_policy="auto")
                    (install / "settings.json").write_text(json.dumps(settings))
                    installs.append(install)
                    states.append(state)
                retained_endpoint = clients[1].endpoint(2)
                commands, data = root / "bin", root / "data"
                subprocess.run([str(candidate), "install-self", "--base-url", server.origin,
                                "--install-dir", str(commands), "--data-dir", str(data), "--no-completions", "--quiet"],
                               env=environment, capture_output=True, timeout=30, check=True)
                # Force a genuine binary replacement, wrapper repair and hook;
                # subsequent invocations cover the already-current binary path.
                shutil.copyfile(cli, data / "ai-fence-cli-linux-x86_64")
                terminal = dict(environment, AI_FENCE_CLI_AUTO_UPDATE="always", AI_FENCE_CLI_UPDATE_INTERVAL_SECONDS="0")
                def launch(extra=None):
                    result = subprocess.run([str(commands / "ai-fence-cli"), "--version"],
                        env=dict(terminal, **(extra or {})), capture_output=True, text=True, timeout=90)
                    self.assertEqual(result.returncode, 0, "terminal auto-update failed")
                    self.assertEqual(result.stdout.strip(), "ai-fence-cli " + version)
                launch()
                clients[0].process.wait(timeout=12)
                clients[0].close()
                clients.pop(0)
                request = json.loads((installs[0] / "reconnect-request.json").read_text())
                self.assertTrue(request["automatic"])
                self.assertFalse((installs[1] / "reconnect-request.json").exists())
                self.assertEqual(clients[0].endpoint(3), retained_endpoint)
                settings = json.loads((installs[1] / "settings.json").read_text())
                self.assertEqual(settings["cli_sha256"], candidate_hash)
                self.assertEqual(settings["reconnect_policy"], "off")
                # Enable the saved policy to distinguish the session-only env
                # opt-out, then prove a later normal terminal launch converges.
                settings["reconnect_policy"] = "auto"
                (installs[1] / "settings.json").write_text(json.dumps(settings))
                launch({"AI_FENCE_CODEX_RECONNECT": "off"})
                self.assertEqual(clients[0].endpoint(4), retained_endpoint)
                launch()
                clients[0].process.wait(timeout=12)
                clients[0].close()
                clients.clear()
                for state in states:
                    self.assertIn("fixture-invalid-no-inference", (state / "auth.json").read_text())
            finally:
                for client in clients:
                    client.close(terminate=True)


if __name__ == "__main__":
    unittest.main()
