"""Actual native external-auth API, isolated fake credentials, no inference."""
import base64
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from http.server import BaseHTTPRequestHandler
from tls_fixture import start_tls

import test_native as native_fixture

ACCOUNT = "11111111-1111-4111-8111-111111111111"


def jwt(label):
    def encode(value):
        return base64.urlsafe_b64encode(json.dumps(value).encode()).decode().rstrip("=")
    claims = {"sub":"fixture", "email":"fixture@example.invalid", "exp":int(time.time()) + 3600,
              "jti":label, "https://api.openai.com/auth":{
                  "chatgpt_account_id":ACCOUNT, "chatgpt_plan_type":"plus"}}
    return encode({"alg":"RS256", "typ":"JWT"}) + "." + encode(claims) + ".fixture-signature"


@unittest.skipUnless(os.environ.get("AF_TEST_CLI") and os.environ.get("AF_TEST_NATIVE"), "set AF_TEST_CLI and AF_TEST_NATIVE")
class NativeSharedAuth(unittest.TestCase):
    def test_native_default_login_is_used_with_existing_revoked_profile_and_history_unchanged(self):
        with tempfile.TemporaryDirectory(prefix="af-native-auth-") as temporary:
            root = Path(temporary)
            home, state, owner = root / "home", root / "state", root / "native"
            for path in [home, state, owner]:
                path.mkdir()
            # Codex 0.163 performs workspace routing discovery during login and
            # account/read. Serve its real schema over local TLS, so fake auth
            # never depends on or contacts the real provider.
            paths, authenticated = [], []
            class Handler(BaseHTTPRequestHandler):
                def do_GET(self):
                    paths.append(self.path.split("?", 1)[0])
                    if "accounts/check" in self.path:
                        valid = self.headers.get("Authorization") == "Bearer " + expected_access
                        authenticated.append(valid)
                        if not valid:
                            self.send_response(401)
                            self.end_headers()
                            return
                    entry = {"id":ACCOUNT, "name":"Fixture", "plan_type":"plus", "structure":"personal",
                             "workspace_backend_origin":origin, "account_routing_override":"NO_CONSTRAINT",
                             "profile_picture_url":None}
                    response = {"accounts":[entry],
                                "account_ordering":[ACCOUNT], "default_account_id":ACCOUNT,
                                } if "accounts/check" in self.path else {}
                    payload = json.dumps(response).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                def log_message(self, *args):
                    pass
            server, cert = start_tls(root, Handler)
            origin = server.origin
            self.addCleanup(server.server_close)
            self.addCleanup(server.shutdown)
            def auth(label):
                return json.dumps({"auth_mode":"chatgpt", "tokens":{
                    "access_token":jwt(label), "id_token":jwt(label),
                    "refresh_token":"fixture-no-refresh", "account_id":ACCOUNT},
                    "last_refresh":"2026-10-18T00:00:00Z"})
            stale = auth("profile-revoked")
            fresh = auth("native-fresh")
            expected_access = json.loads(fresh)["tokens"]["access_token"]
            (state / "auth.json").write_text(stale)
            (owner / "auth.json").write_text(fresh)
            config = 'model="fixture-model"\nchatgpt_base_url=' + json.dumps(origin) + '\n'
            (state / "config.toml").write_text(config)
            (owner / "config.toml").write_text('cli_auth_credentials_store="file"\nchatgpt_base_url=' + json.dumps(origin) + '\n')
            environment = {k:v for k,v in os.environ.items() if not k.startswith(("AI_FENCE_", "OPENAI_", "ANTHROPIC_", "CODEX_")) and not k.upper().endswith("_PROXY")}
            environment.update(HOME=str(home), CODEX_HOME=str(owner), XDG_CONFIG_HOME=str(root / "config"),
                               AI_FENCE_HOME=str(root / "fence"), AI_FENCE_SNAPSHOTS="off", OTEL_SDK_DISABLED="true")
            environment["CODEX_CA_CERTIFICATE"] = str(cert)
            environment["SSL_CERT_FILE"] = str(cert)
            cli, native = str(Path(os.environ["AF_TEST_CLI"]).resolve()), str(Path(os.environ["AF_TEST_NATIVE"]).resolve())
            install = root / "adapter"
            setup = [cli, "codex-desktop", "--install-root", str(install), "setup", "--native-codex", native,
                     "--state-home", str(state), "--profile", "fixture", "--updates", "off"]
            result = subprocess.run(setup, env=environment, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, "isolated native setup failed: " + result.stderr.decode()[:500])
            settings = json.loads((install / "settings.json").read_text())
            self.assertEqual(settings["auth_home"], str(owner))
            self.assertEqual(settings["auth_mode"], "native-owner")
            client = native_fixture.Client(install / "bin/codex", environment)
            try:
                try:
                    account = client.request(2, "account/read", {"refreshToken":False})
                except AssertionError:
                    raise AssertionError("Native fixture auth classification: " + client.last_error.get("message", "unknown") + "; mock paths=" + repr(paths)) from None
                self.assertIsNotNone(account["account"])
                effective = client.request(3, "config/read", {"includeLayers":False})["config"]
                self.assertEqual(effective["cli_auth_credentials_store"], "ephemeral")
                self.assertEqual(effective["model_provider"], "ai_fence")
                self.assertEqual((state / "auth.json").read_text(), stale)
                self.assertEqual((owner / "auth.json").read_text(), fresh)
                project = root / "project"
                project.mkdir()
                thread = client.request(4, "thread/start", {"cwd":str(project)})["thread"]
                self.assertEqual(thread["modelProvider"], "ai_fence")
                client.request(5, "thread/inject_items", {"threadId":thread["id"], "items":[
                    {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"Synthetic fixture history."}]}]})
                client.request(6, "account/logout", {})
                self.assertFalse((owner / "auth.json").exists())
                self.assertEqual((state / "auth.json").read_text(), stale)
            finally:
                client.close()
            # A reconnect uses the same history store and independently installs
            # the latest source access token, without rewriting either bundle.
            (owner / "auth.json").write_text(fresh)  # Simulate a fresh native login in this scratch fixture.
            client = native_fixture.Client(install / "bin/codex", environment)
            try:
                resumed = client.request(5, "thread/read", {"threadId":thread["id"], "includeTurns":False})
                self.assertEqual(resumed["thread"]["id"], thread["id"])
            finally:
                client.close()
            # Session-only opt-out keeps the same history and credential owner;
            # it must not fall back to the revoked profile credential file.
            client = native_fixture.Client(install / "bin/codex", dict(environment, AI_FENCE_CODEX_DESKTOP="0"))
            try:
                effective = client.request(7, "config/read", {"includeLayers":False})["config"]
                self.assertEqual(effective["model_provider"], "openai")
                self.assertEqual(effective["cli_auth_credentials_store"], "ephemeral")
                self.assertIsNotNone(client.request(8, "account/read", {"refreshToken":False})["account"])
                saved = client.request(9, "thread/read", {"threadId":thread["id"], "includeTurns":False})
                self.assertEqual(saved["thread"]["id"], thread["id"])
                report = subprocess.run([cli, "codex-desktop", "--install-root", str(install), "doctor", "--strict"],
                    env=environment, capture_output=True, timeout=15)
                self.assertEqual(report.returncode, 0, report.stderr.decode()[:500])
                launches = json.loads(report.stdout)["launches"]
                self.assertTrue(any(item.get("mode") == "native-stdio" and not item["fenced"] for item in launches))
            finally:
                client.close()
            self.assertEqual((state / "config.toml").read_text(), config)
            self.assertEqual((state / "auth.json").read_text(), stale)
            self.assertEqual((owner / "auth.json").read_text(), fresh)
            self.assertFalse(list((install / "launches").glob("*.json")))
            self.assertTrue(any("accounts/check" in path for path in paths))
            self.assertTrue(authenticated)
            self.assertTrue(all(authenticated), "native requests must use the fresh owner's token, never the revoked profile token")
            self.assertFalse(any(path.endswith("/responses") for path in paths))
