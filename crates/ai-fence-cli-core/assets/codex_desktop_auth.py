"""Native Codex owns OAuth; a separate history process receives access tokens.

No refresh token is copied, logged, persisted in another home or sent to the
desktop client. All login/refresh operations use the native account API in the
explicit credential home. Requires native file storage and external-token API.
"""
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import stat
import subprocess
import threading
import uuid


def provider_check(settings, client_version, opener=None):
    """Explicit read-only check, never OAuth refresh, inference or body logging."""
    from urllib.request import Request, urlopen
    from urllib.error import HTTPError
    try:
        tokens = access_tokens(settings)
        if not tokens:
            return dict(status="sign_in_required", authenticated=False)
        request = Request("https://chatgpt.com/backend-api/codex/models?client_version=" + client_version,
                          headers={"Authorization":"Bearer " + tokens["accessToken"],
                                   "ChatGPT-Account-Id":tokens["chatgptAccountId"],
                                   "User-Agent":"codex_cli_rs/" + client_version, "Originator":"codex_cli_rs"})
        with (opener or urlopen)(request, timeout=8) as response:
            return dict(status="ok" if response.status == 200 else "provider_error",
                        http_status=response.status, authenticated=response.status == 200)
    except HTTPError as error:
        return dict(status="sign_in_required" if error.code == 401 else "provider_error",
                    http_status=error.code, authenticated=False)
    except (AuthError, OSError, ValueError):
        return dict(status="unavailable", authenticated=False)


class AuthError(RuntimeError):
    def __init__(self, message="Native sign-in is required in the selected credential home", code=-32001):
        super().__init__(message)
        self.code = code


def directory_identity(home):
    try:
        info = Path(home).stat()
    except OSError:
        raise AuthError("Credential home is missing; reattach its filesystem without creating a replacement") from None
    if not stat.S_ISDIR(info.st_mode):
        raise AuthError("Credential home must be a directory")
    return dict(device=info.st_dev, inode=info.st_ino)


def validate_home(settings):
    if settings.get("auth_identity") and directory_identity(settings["auth_home"]) != settings["auth_identity"]:
        raise AuthError("Credential home identity changed; select it explicitly after inspecting the account")


def initialize(settings, value):
    params = value.get("params")
    if not isinstance(params, dict) or not isinstance(params.get("clientInfo"), dict):
        raise AuthError("Initialize requires native clientInfo parameters", code=-32602)
    capabilities = params.get("capabilities")
    if capabilities is None:
        capabilities = params["capabilities"] = {}
    if not isinstance(capabilities, dict):
        raise AuthError("Initialize capabilities must be an object", code=-32602)
    settings["_auth_client_info"] = dict(params["clientInfo"])
    capabilities["experimentalApi"] = True


def auth_file(home):
    path = Path(home) / "auth.json"
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return None
    except OSError:
        raise AuthError("Cannot read the native credential file; inspect its ownership") from None
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022 or info.st_size > 1024 * 1024:
            raise AuthError("Native credential file must be an owned, bounded regular file")
        try:
            value = json.load(stream)
        except (ValueError, UnicodeError):
            raise AuthError("Native credential file is invalid; sign in through the selected owner") from None
    if not isinstance(value, dict):
        raise AuthError("Native credential file is invalid")
    return value


def account_key(bundle):
    tokens = (bundle or {}).get("tokens") or {}
    account = tokens.get("account_id") if isinstance(tokens, dict) else None
    return hashlib.sha256(account.encode()).hexdigest() if isinstance(account, str) and account else None


def policy(state_home, auth_home, expected_key=None):
    state_home, auth_home = Path(state_home).resolve(), Path(auth_home).resolve()
    identity = directory_identity(auth_home)
    if state_home == auth_home:
        return dict(auth_mode="profile", auth_home=str(state_home), auth_account_key=None, auth_identity=identity)
    config = auth_home / "config.toml"
    if config.exists():
        import tomllib
        try:
            settings = tomllib.loads(config.read_text())
        except (OSError, ValueError):
            raise AuthError("Cannot inspect native credential storage configuration") from None
        if settings.get("cli_auth_credentials_store", "file") != "file":
            raise AuthError("Shared native auth currently requires file credential storage; select the profile auth home for keyring/auto storage")
    native, profile = auth_file(auth_home), auth_file(state_home)
    native_key, profile_key = account_key(native), account_key(profile)
    if profile and (not profile_key or (native and profile_key != native_key)):
        raise AuthError("Credential homes select different or unidentifiable accounts; keep the profile auth home or select a separate history profile")
    if native and not native_key:
        raise AuthError("Shared native auth requires an identifiable ChatGPT account; API keys use the profile auth home")
    if not (native_key or profile_key or expected_key):
        raise AuthError("Sign in to native Codex before binding its credential home, or select the profile auth home")
    return dict(auth_mode="native-owner", auth_home=str(auth_home), auth_account_key=native_key or profile_key or expected_key,
                auth_identity=identity)


def access_tokens(settings, previous_account=None):
    validate_home(settings)
    bundle = auth_file(settings["auth_home"])
    if bundle is None:
        return None
    tokens = bundle.get("tokens") or {}
    if not isinstance(tokens, dict):
        raise AuthError()
    token, account = tokens.get("access_token"), tokens.get("account_id")
    if not isinstance(token, str) or not token or not isinstance(account, str) or not account:
        raise AuthError()
    expected = settings.get("auth_account_key")
    if (expected and expected != account_key(bundle)) or (previous_account and account != previous_account):
        raise AuthError("Native login selected another account; preserve this history profile and select its original account")
    return dict(accessToken=token, chatgptAccountId=account)


class NativeOwner:
    """A bounded, auth-only native app-server; never opens a conversation."""
    def __init__(self, settings, notify):
        validate_home(settings)
        self.notify, self.pending = notify, {}
        self.gate = threading.Lock()
        environment = os.environ.copy()
        environment["CODEX_HOME"] = settings["auth_home"]
        for key in ["CODEX_INSTALL_DIR", "CODEX_CLI_PATH", "_AI_FENCE_DESKTOP_OWNED"]:
            environment.pop(key, None)
        self.child = subprocess.Popen([settings["native_codex"], "-c", 'model_provider="openai"', "app-server"],
            env=environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, start_new_session=True)
        threading.Thread(target=self.read, daemon=True).start()
        try:
            self.call("initialize", dict(clientInfo=settings.get("_auth_client_info", dict(name="ai_fence_auth_owner", version="1"))))
            self.send(dict(method="initialized", params={}))
        except BaseException:
            self.close()
            raise

    def send(self, value):
        with self.gate:
            self.child.stdin.write(json.dumps(value).encode() + b"\n")
            self.child.stdin.flush()

    def call(self, method, params):
        identifier = "af_owner_" + uuid.uuid4().hex
        result = queue.Queue(maxsize=1)
        with self.gate:
            self.pending[identifier] = result
        try:
            self.send(dict(id=identifier, method=method, params=params))
            try:
                response = result.get(timeout=6)
            except queue.Empty:
                raise AuthError("Native authentication operation timed out; retry sign-in explicitly") from None
            if response is None:
                raise AuthError("Native credential owner disconnected during authentication")
            if "error" in response:
                message = response["error"].get("message", "")
                reason = next((label for text, label in [
                    ("unauthorized", "sign_in_required"), ("revoked", "sign_in_required"),
                    ("missing backend origin", "workspace_backend_missing"),
                    ("selected workspace missing", "workspace_not_found"),
                    ("invalid account routing", "workspace_route_invalid"),
                    ("must use an HTTPS origin", "workspace_origin_invalid"),
                    ("conflicts with workspace routing", "workspace_configuration_conflict"),
                    ("configuration changed", "workspace_configuration_changed")]
                    if text in message), "account_api_rejected")
                raise AuthError("Native credential owner rejected authentication: " + reason)
            return response.get("result", {})
        finally:
            with self.gate:
                self.pending.pop(identifier, None)

    def read(self):
        try:
            for line in self.child.stdout:
                if len(line) > 1024 * 1024:
                    break
                value = json.loads(line)
                with self.gate:
                    pending = self.pending.get(value.get("id"))
                if pending:
                    pending.put_nowait(value)
                elif value.get("method", "").startswith("account/") and "id" not in value:
                    self.notify(value)
        except (OSError, ValueError, queue.Full):
            pass
        finally:
            with self.gate:
                for pending in self.pending.values():
                    if pending.empty():
                        pending.put_nowait(None)

    def close(self):
        if self.child.poll() is None:
            try:
                os.killpg(self.child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        try:
            self.child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(self.child.pid, signal.SIGKILL)
            self.child.wait(timeout=2)
        self.child.stdin.close()
        self.child.stdout.close()


class Bridge:
    def __init__(self, settings, native_send, client_send, owner_factory=NativeOwner):
        self.settings, self.native_send, self.client_send = settings, native_send, client_send
        self.owner_factory, self.owner = owner_factory, None
        self.gate, self.owner_gate = threading.Lock(), threading.Lock()
        self.pending, self.closed = {}, False
        self.notifications = queue.Queue(maxsize=64)
        self.current = None
        threading.Thread(target=self.notices, daemon=True).start()

    def credential_owner(self):
        validate_home(self.settings)
        with self.owner_gate:
            if self.closed:
                raise AuthError("Desktop authentication connection is closed")
            if self.owner is None:
                self.owner = self.owner_factory(self.settings, self.notifications.put_nowait)
            return self.owner

    def runtime_call(self, method, params):
        identifier = "af_runtime_auth_" + uuid.uuid4().hex
        result = queue.Queue(maxsize=1)
        with self.gate:
            self.pending[identifier] = result
        try:
            self.native_send(dict(id=identifier, method=method, params=params))
            try:
                value = result.get(timeout=6)
            except queue.Empty:
                raise AuthError("Native external-token API is unavailable; check the installed Codex version") from None
            if "error" in value:
                raise AuthError("Native external-token login failed; check the installed Codex version")
            return value.get("result", {})
        finally:
            with self.gate:
                self.pending.pop(identifier, None)

    def sync(self):
        tokens = access_tokens(self.settings)
        if tokens and tokens != self.current:
            self.runtime_call("account/login/start", dict(type="chatgptAuthTokens", **tokens))
            self.current = tokens
        elif tokens is None and self.current is not None:
            self.runtime_call("account/logout", {})
            self.current = None
        return bool(tokens)

    def client(self, value):
        if not value.get("method", "").startswith("account/") or "id" not in value:
            return False
        try:
            if value["method"] == "account/login/start" and (value.get("params") or {}).get("type") not in {"chatgpt", "chatgptDeviceCode"}:
                raise AuthError("Shared native auth supports native ChatGPT sign-in; select profile auth for other login modes")
            result = self.credential_owner().call(value["method"], value.get("params") or {})
            if value["method"] == "account/logout":
                self.runtime_call("account/logout", {})
                self.current = None
            elif value["method"] == "account/read":
                self.sync()
            self.client_send(dict(id=value["id"], result=result))
        except (AuthError, OSError) as error:
            self.client_send(self.error(value["id"], error))
        return True

    @staticmethod
    def error(identifier, error=None):
        return dict(id=identifier, error=dict(code=getattr(error, "code", -32001),
            message="AI Fence authentication: " + (str(error) if isinstance(error, AuthError) else "sign in to the selected credential home"),
            data=dict(origin="ai_fence", stage="auth_owner", retryable=False)))

    def runtime(self, value):
        with self.gate:
            pending = self.pending.get(value.get("id"))
        if pending:
            try:
                pending.put_nowait(value)
            except queue.Full:
                pass
            return True
        if value.get("method") == "account/chatgptAuthTokens/refresh" and "id" in value:
            threading.Thread(target=self.refresh, args=(value,), daemon=True).start()
            return True
        # Only the credential owner reports persistent account/login state.
        return value.get("method") in {"account/updated", "account/login/completed"}

    def refresh(self, value):
        try:
            self.credential_owner().call("account/read", dict(refreshToken=True))
            tokens = access_tokens(self.settings, (value.get("params") or {}).get("previousAccountId"))
            if not tokens:
                raise AuthError()
            self.native_send(dict(id=value["id"], result=tokens))
        except (AuthError, OSError):
            self.native_send(self.error(value["id"]))

    def notices(self):
        while True:
            value = self.notifications.get()
            if value is None:
                return
            if value.get("method") == "account/login/completed" and (value.get("params") or {}).get("success"):
                try:
                    self.sync()
                except (AuthError, OSError):
                    value = dict(method="account/login/completed", params=dict(
                        loginId=value["params"].get("loginId"), success=False,
                        error="Login selected another account or native external-token login failed; preserve this history profile"))
            self.client_send(value)

    def close(self):
        with self.owner_gate:
            self.closed = True
            if self.owner:
                self.owner.close()
        try:
            self.notifications.put_nowait(None)
        except queue.Full:
            pass
