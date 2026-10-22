"""Optional real-binary integration: setup -> adapter -> Fence -> native Codex.

Set AF_TEST_CLI and AF_TEST_NATIVE. Only initialize/config/read are sent; no
inference, user state, user credentials or running application are used.
"""
import json
import os
from pathlib import Path
import queue
import signal
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import unittest


class Client:
    def __init__(self, shim, environment, cwd=None):
        self.process = subprocess.Popen([str(shim), "app-server"], env=environment,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, cwd=cwd)
        self.messages = queue.Queue()
        self.stderr = []
        def read():
            for line in self.process.stdout:
                try:
                    self.messages.put(json.loads(line))
                except ValueError:
                    self.messages.put({"invalid_json": True})
            self.messages.put(None)
        def errors():
            for line in self.process.stderr:
                # Keep child output internal; failures report only exit metadata.
                self.stderr.append(line)
        threading.Thread(target=read, daemon=True).start()
        threading.Thread(target=errors, daemon=True).start()
        try:
            self.request(1, "initialize", {"clientInfo":{"name":"ai_fence_fixture", "version":"1"}})
            self.send({"method":"initialized", "params":{}})
        except BaseException:
            self.close(terminate=True)
            raise

    def send(self, value):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()

    def request(self, identifier, method, parameters):
        self.send(dict(id=identifier, method=method, params=parameters))
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            try:
                value = self.messages.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty:
                detail = b"".join(self.stderr).decode(errors="replace").replace("fixture-invalid-no-inference", "[fixture]")
                def tree(pid, depth=0):
                    path = Path("/proc") / str(pid)
                    try:
                        children = [int(v) for v in (path / "task" / str(pid) / "children").read_text().split()] if depth < 4 else []
                        return dict(pid=pid, name=(path / "comm").read_text().strip(), wait=(path / "wchan").read_text().strip(), children=[tree(v, depth+1) for v in children])
                    except OSError:
                        return dict(pid=pid, gone=True)
                raise AssertionError(f"bounded native RPC timeout; process_metadata={tree(self.process.pid)}; fixture stderr={detail[:1600]}") from None
            if value is None:
                detail = b"".join(self.stderr).decode(errors="replace").replace("fixture-invalid-no-inference", "[fixture]")
                raise AssertionError(f"app-server closed; returncode={self.process.poll()}; fixture stderr={detail[:1600]}")
            if value.get("invalid_json"):
                raise AssertionError("non-JSON text on app-server stdout")
            if value.get("id") == identifier:
                if "error" in value:
                    self.last_error = value["error"]
                    raise AssertionError(f"native RPC rejected {method}; code={value['error'].get('code')}")
                return value["result"]
        raise AssertionError("bounded native RPC timeout")

    def endpoint(self, identifier):
        result = self.request(identifier, "config/read", {"includeLayers":False})
        return result["config"]["model_providers"]["ai_fence"]["base_url"]

    def close(self, terminate=False):
        if terminate:
            self.process.terminate()
        else:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=12)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            raise AssertionError("adapter did not shut down within bounded timeout")
        self.process.stdin.close()
        self.process.stdout.close()
        self.process.stderr.close()


@unittest.skipUnless(os.environ.get("AF_TEST_CLI") and os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_CLI and AF_TEST_NATIVE")
class NativeIntegration(unittest.TestCase):
    def test_dev_channel_survives_prod_desktop_environment_without_changing_history(self):
        with tempfile.TemporaryDirectory(prefix="af-native-dev-channel-") as temporary:
            root = Path(temporary)
            home, state, install = root / "home", root / "state", root / "adapter"
            home.mkdir()
            state.mkdir()
            auth = json.dumps({"auth_mode":"apikey", "OPENAI_API_KEY":"fixture-invalid-no-inference"})
            (state / "auth.json").write_text(auth)
            (state / "history-marker").write_text("keep existing history")
            environment = {k:v for k,v in os.environ.items()
                           if not k.startswith(("AI_FENCE_", "OPENAI_", "ANTHROPIC_", "CODEX_"))}
            environment.update(HOME=str(home), XDG_CONFIG_HOME=str(root / "config"),
                               AI_FENCE_CLI_CHANNEL="prod", AI_FENCE_SNAPSHOTS="off",
                               OPENAI_API_BASE="http://127.0.0.1:1", OTEL_SDK_DISABLED="true")
            cli = str(Path(os.environ["AF_TEST_CLI"]).resolve())
            setup = [cli, "codex-desktop", "--install-root", str(install), "setup", "--channel", "dev",
                     "--update-base-url", "https://dev.example.test", "--native-codex",
                     str(Path(os.environ["AF_TEST_NATIVE"]).resolve()), "--state-home", str(state),
                     "--auth-home", str(state), "--updates", "off", "--reconnect", "off", "--profile", "default"]
            subprocess.run(setup, env=environment, capture_output=True, timeout=30, check=True)
            settings = json.loads((install / "settings.json").read_text())
            self.assertEqual((settings["channel"], settings["profile"]), ("dev", "default"))
            self.assertEqual(settings["update_base_url"], "https://dev.example.test")
            before = (install / "settings.json").read_bytes()
            subprocess.run(setup, env=environment, capture_output=True, timeout=30, check=True)
            self.assertEqual((install / "settings.json").read_bytes(), before)
            client = Client(install / "bin/codex", environment)
            try:
                self.assertTrue(client.endpoint(2).startswith("http://127.0.0.1:"))
                launches = [json.loads(path.read_text()) for path in (install / "launches").glob("*.json")]
                self.assertEqual(len(launches), 1)
                proxy = launches[0]["proxy_pid"]
                values = Path(f"/proc/{proxy}/environ").read_bytes().split(b"\0")
                self.assertIn(b"AI_FENCE_CLI_CHANNEL=dev", values)
                self.assertEqual((state / "auth.json").read_text(), auth)
                self.assertEqual((state / "history-marker").read_text(), "keep existing history")
            finally:
                client.close()

    def test_official_setup_concurrent_endpoints_upgrade_peer_exit_and_sigterm(self):
        with tempfile.TemporaryDirectory(prefix="af-native-setup-") as temporary:
            root = Path(temporary)
            home = root / "home"
            home.mkdir()
            state = root / "state"
            state.mkdir()
            # No secrets or provider sessions are read from the real user home.
            (state / "auth.json").write_text(json.dumps({"auth_mode":"apikey", "OPENAI_API_KEY":"fixture-invalid-no-inference"}))
            config = 'model="fixture-model"\n# keep this user setting\n[notice]\nhide_rate_limit_model_nudge=true\n'
            (state / "config.toml").write_text(config)
            environment = {k:v for k,v in os.environ.items()
                           if not k.startswith(("AI_FENCE_", "OPENAI_", "ANTHROPIC_", "CODEX_"))}
            environment.update(HOME=str(home), XDG_CONFIG_HOME=str(root / "config"),
                               AI_FENCE_HOME=str(root / "fence-state"), AI_FENCE_SNAPSHOTS="off",
                               OPENAI_API_BASE="http://127.0.0.1:1", OTEL_SDK_DISABLED="true")
            cli = str(Path(os.environ["AF_TEST_CLI"]).resolve())
            install = root / "adapter"
            setup = [cli, "codex-desktop", "--install-root", str(install), "setup", "--native-codex",
                     str(Path(os.environ["AF_TEST_NATIVE"]).resolve()), "--state-home", str(state), "--auth-home", str(state), "--updates", "off", "--profile", "fixture-native", "--snapshots", "off"]
            subprocess.run(setup, env=environment, capture_output=True, timeout=20, check=True)
            before = (install / "settings.json").read_bytes()
            subprocess.run(setup, env=environment, capture_output=True, timeout=20, check=True)
            self.assertEqual(before, (install / "settings.json").read_bytes())
            clients = []
            endpoints = []
            try:
                a = Client(install / "bin/codex", environment)
                clients.append(a)
                first = a.endpoint(2)
                endpoints.append(first)
                b = Client(install / "bin/codex", environment)
                clients.append(b)
                second = b.endpoint(2)
                endpoints.append(second)
                self.assertNotEqual(first, second)
                self.assertEqual(a.endpoint(3), first)
                # Use a distinct runnable ELF artifact to exercise an actual
                # release switch, while both original app-servers remain alive.
                upgrade = root / "upgraded-ai-fence-cli"
                shutil.copyfile(cli, upgrade)
                with upgrade.open("ab") as payload:
                    payload.write(b"\nfixture-upgrade-artifact\n")
                upgrade.chmod(0o700)
                subprocess.run([str(upgrade), *setup[1:]], env=environment, capture_output=True, timeout=20, check=True)
                self.assertNotEqual(before, (install / "settings.json").read_bytes())
                self.assertEqual(a.endpoint(4), first)
                c = Client(install / "bin/codex", environment)
                clients.append(c)
                third = c.endpoint(2)
                endpoints.append(third)
                self.assertNotEqual(third, first)
                self.assertEqual(a.endpoint(8), first)
                c.close()
                clients.remove(c)
                b.close()
                clients.remove(b)
                self.assertEqual(a.endpoint(5), first)
                project = root / "project"
                project.mkdir()
                (project / "untracked.txt").write_text("preserved fixture work")
                started = a.request(6, "thread/start", {"cwd":str(project)})
                thread_id = started["thread"]["id"]
                record = json.loads((state / "ai-fence-workspaces" / (thread_id + ".json")).read_text())
                self.assertEqual(record["workspace"]["physical"], str(project))
                self.assertEqual((project / "untracked.txt").read_text(), "preserved fixture work")
                status = subprocess.run([cli, "codex-desktop", "--install-root", str(install), "doctor"],
                                        env=environment, capture_output=True, timeout=10, check=True)
                report = json.loads(status.stdout)
                live = [v for v in report["launches"] if v["alive"]]
                self.assertEqual(len(live), 1)
                self.assertTrue(live[0]["listener_owned"])
                self.assertTrue(live[0]["endpoint_tcp_alive"])
                a.close(terminate=True)
                clients.remove(a)
                self.assertEqual((state / "config.toml").read_text(), config)
                self.assertEqual(json.loads((state / "auth.json").read_text())["OPENAI_API_KEY"], "fixture-invalid-no-inference")
                self.assertFalse(list((install / "launches").glob("*.json")))
                for value in endpoints:
                    port = int(value.split(":")[-1].split("/")[0])
                    with self.assertRaises(OSError):
                        socket.create_connection(("127.0.0.1", port), timeout=0.3)
            finally:
                for client in clients:
                    client.close(terminate=True)


if __name__ == "__main__":
    unittest.main()
