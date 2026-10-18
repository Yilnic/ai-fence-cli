"""One native credential owner; a different home retains conversation state."""
import json
import queue
import unittest
from unittest.mock import patch

import test_desktop as fixtures

desktop = fixtures.desktop


def bundle(token, account="fixture-account"):
    return json.dumps({"auth_mode": "chatgpt", "tokens": {
        "access_token": token, "refresh_token": "fixture-owner-refresh", "account_id": account}})


class SharedAuthTests(unittest.TestCase):
    setUp = fixtures.DesktopTests.setUp

    def test_shared_auth_native_tui_opt_out_fails_before_creating_launch_metadata(self):
        native = self.root / "native"
        native.touch()
        self.settings.update(auth_mode="native-owner", native_codex=str(native))
        with patch.dict(desktop.os.environ, AI_FENCE_CODEX_DESKTOP="0"):
            with self.assertRaisesRegex(RuntimeError, "stdio app-server"):
                desktop.adapter(self.settings, [])
        self.assertFalse((self.root / "install/launches").exists())

    def test_fresh_native_login_ignores_existing_revoked_profile_without_copying(self):
        owner = self.root / "native"
        owner.mkdir()
        stale = bundle("fixture-revoked")
        (self.home / "auth.json").write_text(stale)
        (owner / "auth.json").write_text(bundle("fixture-fresh"))
        policy = desktop.auth_policy(self.home, owner)
        self.assertEqual(policy["auth_home"], str(owner))
        self.assertEqual(policy["auth_mode"], "native-owner")
        settings = {**self.settings, **policy}
        tokens = desktop.auth_runtime.access_tokens(settings)
        self.assertEqual(tokens["accessToken"], "fixture-fresh")
        self.assertEqual((self.home / "auth.json").read_text(), stale)
        self.assertFalse((self.home / "auth.json").is_symlink())

    def test_atomic_owner_rotation_and_account_switch_are_not_stale_copies(self):
        owner = self.root / "native"
        owner.mkdir()
        (owner / "auth.json").write_text(bundle("fixture-first"))
        settings = {**self.settings, **desktop.auth_policy(self.home, owner)}
        rotated = owner / "rotated"
        rotated.write_text(bundle("fixture-second"))
        rotated.replace(owner / "auth.json")
        self.assertEqual(desktop.auth_runtime.access_tokens(settings)["accessToken"], "fixture-second")
        self.assertFalse((self.home / "auth.json").exists())
        (owner / "auth.json").write_text(bundle("fixture-foreign", "another-account"))
        with self.assertRaisesRegex(RuntimeError, "account"):
            desktop.auth_runtime.access_tokens(settings)

    def test_different_accounts_cannot_be_implicitly_bound_to_one_history_home(self):
        owner = self.root / "native"
        owner.mkdir()
        (owner / "auth.json").write_text(bundle("fixture-fresh", "another-account"))
        (self.home / "auth.json").write_text(bundle("fixture-stale"))
        with self.assertRaisesRegex(RuntimeError, "account"):
            desktop.auth_policy(self.home, owner)

    def test_login_reconnect_refresh_and_logout_use_one_owner_and_hide_internal_tokens(self):
        owner_home = self.root / "native"
        owner_home.mkdir()
        (owner_home / "auth.json").write_text(bundle("fixture-initial"))
        stale = bundle("fixture-revoked")
        (self.home / "auth.json").write_text(stale)
        history = self.home / "sessions"
        history.mkdir()
        (history / "fixture").write_text("preserved")
        settings = {**self.settings, **desktop.auth_policy(self.home, owner_home)}
        sent, received = [], queue.Queue()
        owners = []

        class Owner:
            def __init__(self, configured, notify):
                self.calls, self.notify, self.closed = [], notify, False
                self.asserted_home = configured["auth_home"]
                owners.append(self)

            def call(self, method, params):
                self.calls.append((method, params))
                if method == "account/login/start":
                    (owner_home / "auth.json").write_text(bundle("fixture-new-login"))
                    self.notify(dict(method="account/login/completed", params=dict(loginId="fixture-login", success=True)))
                    return dict(type="chatgpt", loginId="fixture-login", authUrl="https://example.invalid/fixture")
                if method == "account/read" and params.get("refreshToken"):
                    (owner_home / "auth.json").write_text(bundle("fixture-refreshed"))
                if method == "account/logout":
                    (owner_home / "auth.json").unlink()
                return {}

            def close(self):
                self.closed = True

        def send(value):
            sent.append(value)
            if str(value.get("id", "")).startswith("af_runtime_auth_"):
                bridge.runtime(dict(id=value["id"], result={}))

        bridge = desktop.auth_runtime.Bridge(settings, send, received.put, owner_factory=Owner)
        self.addCleanup(bridge.close)
        bridge.sync()
        self.assertEqual(sent[0]["params"]["accessToken"], "fixture-initial")
        self.assertTrue(bridge.client(dict(id=7, method="account/login/start", params=dict(type="chatgpt"))))
        events = [received.get(timeout=2), received.get(timeout=2)]
        self.assertTrue(any(event.get("params", {}).get("success") for event in events))
        self.assertEqual(owners[0].asserted_home, str(owner_home))
        bridge.refresh(dict(id=8, method="account/chatgptAuthTokens/refresh", params=dict(previousAccountId="fixture-account")))
        self.assertEqual(sent[-1]["result"]["accessToken"], "fixture-refreshed")
        self.assertEqual(owners[0].calls[-1], ("account/read", {"refreshToken": True}))
        self.assertNotIn("accessToken", json.dumps(events))
        bridge.client(dict(id=9, method="account/logout"))
        self.assertFalse((owner_home / "auth.json").exists())
        self.assertEqual((self.home / "auth.json").read_text(), stale)
        self.assertEqual((history / "fixture").read_text(), "preserved")
        self.assertEqual(sent[-1]["method"], "account/logout")

    def test_failed_refresh_is_one_local_auth_error_without_transient_retry(self):
        owner = self.root / "native"
        owner.mkdir()
        (owner / "auth.json").write_text(bundle("fixture-token"))
        settings = {**self.settings, **desktop.auth_policy(self.home, owner)}
        calls, replies = [], []
        class Failed:
            def __init__(self, settings, notify):
                pass
            def call(self, method, params):
                calls.append(method)
                raise desktop.auth_runtime.AuthError()
            def close(self):
                pass
        bridge = desktop.auth_runtime.Bridge(settings, replies.append, lambda value:None, owner_factory=Failed)
        self.addCleanup(bridge.close)
        bridge.refresh(dict(id=10, params={}))
        self.assertEqual(calls, ["account/read"])
        self.assertEqual(replies[0]["error"]["data"], dict(origin="ai_fence", stage="auth_owner", retryable=False))

    def test_missing_or_replaced_credential_home_is_not_recreated(self):
        owner = self.root / "native"
        owner.mkdir()
        (owner / "auth.json").write_text(bundle("fixture-token"))
        settings = {**self.settings, **desktop.auth_policy(self.home, owner)}
        owner.rename(self.root / "original-native")
        with self.assertRaisesRegex(RuntimeError, "missing"):
            desktop.auth_runtime.access_tokens(settings)
        self.assertFalse(owner.exists())
        owner.mkdir()
        (owner / "auth.json").write_text(bundle("fixture-token"))
        with self.assertRaisesRegex(RuntimeError, "identity"):
            desktop.auth_runtime.access_tokens(settings)

    def test_initialize_retains_identity_and_null_capabilities_have_a_valid_protocol_error(self):
        value = dict(method="initialize", params=dict(clientInfo=dict(name="fixture", version="1"), capabilities=None))
        desktop.auth_runtime.initialize(self.settings, value)
        self.assertEqual(self.settings["_auth_client_info"], dict(name="fixture", version="1"))
        self.assertTrue(value["params"]["capabilities"]["experimentalApi"])
        value["params"]["capabilities"] = "invalid"
        with self.assertRaises(desktop.auth_runtime.AuthError) as raised:
            desktop.auth_runtime.initialize(self.settings, value)
        self.assertEqual(desktop.auth_runtime.Bridge.error(1, raised.exception)["error"]["code"], -32602)
