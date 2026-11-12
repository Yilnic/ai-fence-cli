"""Launch-bound immutable CLI updates; no service, timer or active restart."""
import base64
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePath
import platform
import re
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit
from urllib.request import Request, urlopen

_auth_spec = importlib.util.spec_from_file_location("ai_fence_update_identity", Path(__file__).with_name("codex_desktop_auth.py"))
auth_identity = importlib.util.module_from_spec(_auth_spec)
_auth_spec.loader.exec_module(auth_identity)


class CandidateError(ValueError):
    def __init__(self, report):
        super().__init__("candidate desktop bootstrap did not pass")
        self.report = report


def bootstrap_check(settings, timeout=25):
    """Exercise the candidate bridge, never the healthy connection or history.

    The saved admission prefix encloses the adapter and both native children.
    A startup/admission timeout is deliberately not a sign-in diagnosis.
    Only fixed stage/classification metadata leaves this function.
    """
    stage, child = "startup", None
    report = dict(status="failed", stage=stage, classification="startup_failed",
                  release=settings["release"], native_version=settings.get("native_version", "unknown"), checked_at=time.time())
    runtime = Path(settings["install_root"]) / "releases" / settings["release"] / "runtime.py"
    with tempfile.TemporaryDirectory(prefix="af-bootstrap-") as temporary:
        home = Path(temporary) / "state"
        home.mkdir(mode=0o700)
        isolated = dict(settings, install_root=str(Path(temporary) / "adapter"),
                        state_home=str(home), seed_home=str(home), roots=[], snapshot_policy="off",
                        readiness_seconds=0, snapshot_directory=None, update_policy="off", native_updates="manual")
        isolated.pop("state_identity", None)
        isolated["format"] = 1
        # An isolated profile never borrows another profile's credential file.
        # Native-owner is required for this check; profile-auth installs retain
        # their current auth boundary and are checked by their existing tests.
        if settings.get("auth_mode") != "native-owner":
            return dict(report, status="not_applicable", classification="profile_auth")
        Path(isolated["install_root"]).mkdir(mode=0o700)
        save(Path(isolated["install_root"]) / "settings.json", isolated)
        releases = Path(isolated["install_root"]) / "releases"
        releases.mkdir(mode=0o700)
        (releases / settings["release"]).symlink_to(runtime.parent, target_is_directory=True)
        environment = os.environ.copy()
        environment.update(_AI_FENCE_DESKTOP_SETTINGS=json.dumps(isolated), AI_FENCE_CODEX_DESKTOP="1",
                           AI_FENCE_SNAPSHOTS="off", AI_FENCE_CLI_AUTO_UPDATE="never", PYTHONDONTWRITEBYTECODE="1",
                           HOME=temporary, AI_FENCE_HOME=str(Path(temporary) / "fence"),
                           XDG_CONFIG_HOME=str(Path(temporary) / "config"), XDG_DATA_HOME=str(Path(temporary) / "data"),
                           XDG_CACHE_HOME=str(Path(temporary) / "cache"))
        for key in ["_AI_FENCE_DESKTOP_OWNED", "CODEX_INSTALL_DIR", "CODEX_CLI_PATH"]:
            environment.pop(key, None)
        reader = selectors.DefaultSelector()
        try:
            if settings.get("native_sha256"):
                with open(settings["native_codex"], "rb") as stream:
                    if hashlib.file_digest(stream, "sha256").hexdigest() != settings["native_sha256"]:
                        return dict(report, classification="native_payload_changed")
            child = subprocess.Popen([*settings.get("launch_prefix", []), sys.executable, "-B", str(runtime), "adapter", "app-server"],
                env=environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                start_new_session=True, bufsize=0, cwd=temporary)
            reader.register(child.stdout, selectors.EVENT_READ)
            deadline, pending = time.monotonic() + timeout, b""

            def send(value):
                child.stdin.write(json.dumps(value).encode() + b"\n")
                child.stdin.flush()

            def call(identifier, method, params):
                nonlocal pending, stage
                stage = method
                send(dict(id=identifier, method=method, params=params))
                consumed = 0
                while time.monotonic() < deadline:
                    while b"\n" in pending:
                        line, pending = pending.split(b"\n", 1)
                        value = json.loads(line)
                        if not isinstance(value, dict):
                            raise ValueError("invalid protocol envelope")
                        if value.get("id") == identifier and "method" not in value:
                            if "error" in value:
                                raise CandidateError(dict(report, stage=stage, classification="protocol_rejected"))
                            if not isinstance(value.get("result"), dict):
                                raise ValueError("invalid protocol response")
                            return value["result"]
                    if not reader.select(min(0.2, max(0, deadline - time.monotonic()))):
                        continue
                    block = os.read(child.stdout.fileno(), 65536)
                    if not block:
                        raise CandidateError(dict(report, stage=stage, classification="process_exit_or_resource_limit"))
                    consumed += len(block)
                    pending += block
                    if consumed > 2 * 1024 * 1024:
                        raise ValueError("bounded bootstrap response exceeded")
                raise CandidateError(dict(report, stage=stage, classification="startup_or_admission_timeout" if stage == "initialize" else "protocol_timeout"))

            call(1, "initialize", dict(clientInfo=dict(name="ai_fence_bootstrap_check", version="1")))
            legacy = call(2, "getAuthStatus", dict(includeToken=False, refreshToken=False))
            if legacy.get("authMethod") != "chatgpt" or legacy.get("authToken") is not None:
                raise CandidateError(dict(report, stage=stage, classification="owner_auth_not_confirmed"))
            send(dict(method="initialized", params={}))
            account = call(3, "account/read", dict(refreshToken=False))
            if (account.get("account") or {}).get("type") != "chatgpt":
                raise CandidateError(dict(report, stage=stage, classification="owner_auth_not_confirmed"))
            return dict(report, status="ok", stage="account/read", classification="bootstrap_verified")
        except CandidateError as error:
            return error.report
        except (OSError, ValueError, TypeError, subprocess.SubprocessError):
            return dict(report, stage=stage, classification="startup_failed" if stage == "startup" else "protocol_invalid")
        finally:
            reader.close()
            if child is not None:
                if child.poll() is None:
                    # Let the candidate adapter close its own credential-owner
                    # child before escalating only this diagnostic process group.
                    try:
                        os.killpg(child.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=2)
                child.stdin.close()
                child.stdout.close()


def require_bootstrap(settings):
    report = bootstrap_check(settings)
    if report["status"] not in {"ok", "not_applicable"}:
        raise CandidateError(report)
    return report


def version(value):
    if not isinstance(value, str) or not re.fullmatch(r"\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?", value):
        raise ValueError("invalid CLI version")
    return tuple(int(part) for part in value.split("-", 1)[0].split("."))


def channel(value=None):
    if value is None:
        value = "prod"
    if not isinstance(value, str):
        raise ValueError("invalid installation channel")
    value = value.strip().lower() or "prod"
    if not re.fullmatch(r"[a-z0-9][a-z0-9._-]*", value):
        raise ValueError("invalid installation channel")
    return value


def default_base_url(selected=None):
    """Only the two official channels have a public server suggestion."""
    return {"prod": "https://ai.yaaf.de", "dev": "https://ai-dev.yaaf.de"}.get(channel(selected))


def safe_url(value):
    parsed = urlsplit(value)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password or parsed.fragment:
        raise ValueError("release URL requires HTTPS without credentials")
    return parsed


def save(path, value):
    descriptor, temporary = tempfile.mkstemp(prefix=".update-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w") as stream:
            os.fchmod(stream.fileno(), 0o600)
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def setup_arguments(settings):
    result = ["codex-desktop", "--install-root", settings["install_root"], "setup",
              "--channel", channel(settings.get("channel")),
              "--native-codex", settings["native_codex"], "--state-home", settings["state_home"],
              "--auth-home", settings.get("auth_home", settings["state_home"]),
              "--profile", settings["profile"], "--snapshots", settings["snapshot_policy"],
              "--readiness-seconds", str(settings.get("readiness_seconds", 45)),
              "--updates", settings.get("update_policy", "auto"),
              "--reconnect", settings.get("reconnect_policy", "auto"),
              "--update-base-url", settings.get("update_base_url") or default_base_url(settings.get("channel")),
              "--update-interval-seconds", str(settings.get("update_interval_seconds", 86400))]
    for field, flag in [("workspace_registry", "--workspace-registry"), ("snapshot_directory", "--snapshot-directory")]:
        if settings.get(field):
            result += [flag, settings[field]]
    for path in settings.get("roots", []):
        result += ["--approve-root", path]
    if settings.get("native_updates"):
        result += ["--native-updates", settings["native_updates"]]
    return result


def activate(root, expected, staging, native_expected=None):
    """New setup writes immutable releases only; this is the sole live switch."""
    descriptor = os.open(root / ".install.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        current = json.loads((root / "settings.json").read_text())
        proposed = json.loads((staging / "settings.json").read_text())
        if current != expected:
            raise ValueError("installation changed during update")
        if (channel(proposed.get("channel")) != channel(current.get("channel"))
                or (proposed.get("update_base_url") or default_base_url(proposed.get("channel")))
                != (current.get("update_base_url") or default_base_url(current.get("channel")))):
            raise ValueError("automatic update cannot change installation channel or source")
        if proposed.get("native_updates", "manual") != current.get("native_updates", "manual"):
            raise ValueError("automatic update cannot change native update policy")
        native_fields = ["native_codex", "native_version", "native_sha256"]
        if native_expected is not None:
            if current.get("native_updates") != "desktop" or any(proposed.get(key) != native_expected.get(key) for key in native_fields):
                raise ValueError("prepared native runtime differs from the verified desktop runtime")
            for filename, expected_hash in native_expected["files"].items():
                path = Path(filename)
                if path.is_symlink() or not path.is_file() or path.stat().st_mode & 0o077:
                    raise ValueError("native runtime file changed")
                with path.open("rb") as stream:
                    if hashlib.file_digest(stream, "sha256").hexdigest() != expected_hash:
                        raise ValueError("native runtime checksum changed")
        for field in ["install_root", "state_home", "profile", "auth_home", "auth_mode", "auth_account_key",
                      "roots", "snapshot_policy", "snapshot_directory", "workspace_registry", "update_policy", *([] if native_expected else native_fields)]:
            if proposed.get(field) != current.get(field):
                raise ValueError("automatic update cannot change profile or filesystem policy")
        if not auth_identity.same_identity(current.get("state_identity"), proposed.get("state_identity")):
            raise ValueError("automatic update cannot change state filesystem identity")
        if current.get("auth_identity") != proposed.get("auth_identity"):
            if not auth_identity.same_auth_identity(current, proposed.get("auth_identity") or {}):
                raise ValueError("automatic update cannot change credential filesystem identity")
        if proposed.get("launch_prefix", []) != current.get("launch_prefix", []):
            raise ValueError("automatic update cannot change admission policy")
        if proposed.get("reconnect_policy", "auto") != current.get("reconnect_policy", "auto"):
            raise ValueError("automatic update cannot change reconnect policy")
        old_mutable = {name for name in current.get("files", {}) if not name.startswith("releases/")}
        new_mutable = {name for name in proposed.get("files", {}) if not name.startswith("releases/")}
        if old_mutable != new_mutable:
            raise ValueError("discovery router upgrade requires explicit setup")
        for name, expected_hash in proposed.get("files", {}).items():
            relative = PurePath(name)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("invalid prepared release path")
            path = root / name
            if path.is_symlink() or not path.is_file():
                raise ValueError("prepared release file missing or changed")
            with path.open("rb") as stream:
                if hashlib.file_digest(stream, "sha256").hexdigest() != expected_hash:
                    raise ValueError("prepared release integrity failed")
            if name in new_mutable and current["files"][name] != expected_hash:
                raise ValueError("discovery router upgrade requires explicit setup")
        if (staging / "snapshot-roots.json").exists() or (root / "snapshot-roots.json").exists():
            if (staging / "snapshot-roots.json").read_bytes() != (root / "snapshot-roots.json").read_bytes():
                raise ValueError("automatic update cannot change checkpoint approval")
        try:
            report = require_bootstrap(proposed)
        except CandidateError as error:
            save(root / "bootstrap-status.json", error.report)
            raise
        save(root / "bootstrap-status.json", report)
        save(root / "previous-settings.json", current)
        save(root / "settings.json", proposed)
    finally:
        os.close(descriptor)


DESKTOP_RUNTIME_DISCOVERY = r'''
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$taskPackages=@(Get-AppxPackage -Name OpenAI.Codex | Where-Object {
    $_.PackageFamilyName -eq 'OpenAI.Codex_2p2nqsd0c76g0' -and $_.SignatureKind -eq 'Store' -and $_.Status -eq 'Ok'
})
$taskCandidates=@(Get-Process ChatGPT -ErrorAction SilentlyContinue | Where-Object {$_.MainWindowHandle -ne 0} | ForEach-Object {
    $taskExe=$_.Path
    foreach($taskPackage in $taskPackages){
        if($taskExe -eq (Join-Path $taskPackage.InstallLocation 'app\ChatGPT.exe')){
            Join-Path $taskPackage.InstallLocation 'app\resources'
        }
    }
} | Select-Object -Unique)
if($taskCandidates.Count -ne 1){throw 'desktop_runtime_not_unique'}
$taskResources=$taskCandidates[0]
$taskVersion=(& (Join-Path $taskResources 'codex.exe') --version 2>$null | Out-String).Trim()
if($LASTEXITCODE -ne 0){throw 'desktop_native_version_unavailable'}
@{resources=$taskResources;version=$taskVersion} | ConvertTo-Json -Compress
'''


def desktop_runtime(run=subprocess.run):
    """Find the running Store app, rather than an arbitrary/latest installed app."""
    # Discovery/update helpers must never consume the owning stdio RPC stream.
    # WSL interop otherwise drains early initialize messages into PowerShell.
    powershell = shutil.which("powershell.exe")
    if not powershell or not os.environ.get("WSL_INTEROP"):
        raise ValueError("windows_desktop_unavailable")
    shell_path = run(["/usr/bin/wslpath", "-w", "-a", powershell], stdin=subprocess.DEVNULL, capture_output=True, timeout=3)
    if (shell_path.returncode or len(shell_path.stdout) > 16384
            or not re.fullmatch(r"[A-Za-z]:\\Windows\\System32\\WindowsPowerShell\\v1\.0\\powershell\.exe",
                                shell_path.stdout.decode().strip(), re.IGNORECASE)):
        raise ValueError("windows_desktop_unavailable")
    script = base64.b64encode(DESKTOP_RUNTIME_DISCOVERY.encode("utf-16-le")).decode()
    result = run([powershell, "-NoProfile", "-NonInteractive", "-EncodedCommand", script],
                 stdin=subprocess.DEVNULL, capture_output=True, timeout=10)
    if result.returncode or len(result.stdout) > 16384:
        raise ValueError("windows_desktop_unavailable")
    candidate = json.loads(result.stdout.decode("utf-8-sig"))
    if (not isinstance(candidate, dict) or not isinstance(candidate.get("resources"), str)
            or not re.fullmatch(r"[A-Za-z]:\\[^\r\n]*\\OpenAI\.Codex_[^\\]+__2p2nqsd0c76g0\\app\\resources", candidate["resources"])):
        raise ValueError("windows_desktop_unavailable")
    native_version_key(candidate.get("version"))
    translated = run(["/usr/bin/wslpath", "-u", "-a", candidate["resources"]], stdin=subprocess.DEVNULL, capture_output=True, timeout=3)
    if translated.returncode or len(translated.stdout) > 16384:
        raise ValueError("windows_desktop_unavailable")
    resources = Path(translated.stdout.decode().strip())
    if not resources.is_absolute():
        raise ValueError("windows_desktop_unavailable")
    return dict(resources=resources, version=candidate["version"])


def native_version_key(value):
    match = re.fullmatch(r"codex-cli (\d+)\.(\d+)\.(\d+)(?:-([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*))?", value or "")
    if not match:
        raise ValueError("invalid_native_version")
    prerelease = match[4]
    identifiers = tuple((0, int(part)) if part.isdecimal() else (1, part) for part in (prerelease or "").split("."))
    return tuple(int(match[index]) for index in (1, 2, 3)), prerelease is None, identifiers


NATIVE_FILES = {"codex": "bin/codex", "codex-code-mode-host": "bin/codex-code-mode-host", "rg": "codex-path/rg"}


def native_hash(path, private=False):
    """Reject wrappers/PE files and partial copies before executing a payload."""
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb") as stream:
        before = os.fstat(stream.fileno())
        if (not stat.S_ISREG(before.st_mode) or not 20 <= before.st_size <= 512 * 1024 * 1024
                or private and (before.st_uid != os.geteuid() or before.st_mode & 0o077)):
            raise ValueError("invalid_native_payload")
        header = stream.read(20)
        if header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != 62:
            raise ValueError("invalid_native_payload")
        stream.seek(0)
        checksum = hashlib.file_digest(stream, "sha256").hexdigest()
        after = os.fstat(stream.fileno())
        if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
            raise ValueError("native_payload_changed")
        return checksum


def native_check(settings, script, check_only=False, discover=desktop_runtime, run=subprocess.run):
    """Adopt a verified desktop runtime only when no history adapter is running.

    The lifecycle lock also serializes the final settings read in adapter startup.
    No native process, connection, goal or user credential is stopped or replaced.
    """
    if settings.get("native_updates", "manual") != "desktop":
        return dict(status="disabled")
    root = Path(settings["install_root"])
    result = dict(status="failed", checked_at=time.time(), selected_native_version=settings.get("native_version"))
    stage = "discovery"
    descriptors = []
    try:
        descriptor = os.open(root / ".native-update.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        descriptors.append(descriptor)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            result["status"] = "busy"
            return result
        candidate = discover()
        desired = native_version_key(candidate["version"])
        result["desktop_version"] = candidate["version"]
        if desired < native_version_key(settings["native_version"]):
            raise ValueError("desktop_runtime_downgrade")
        stage = "payload"
        sources = {name: Path(candidate["resources"]) / name for name in NATIVE_FILES}
        checksums = {name: native_hash(path) for name, path in sources.items()}
        current = Path(settings["native_codex"]).parent.parent
        if (candidate["version"] == settings["native_version"]
                and all((current / relative).is_file() and native_hash(current / relative) == checksums[name]
                        for name, relative in NATIVE_FILES.items())):
            result["status"] = "current"
        elif check_only:
            result["status"] = "available"
        else:
            stage = "lifecycle"
            lifecycle = os.open(root / ".lifecycle.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
            descriptors.append(lifecycle)
            try:
                fcntl.flock(lifecycle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                result["status"] = "waiting_for_connections"
                return result
            stage = "copy"
            cache = root.with_name(root.name + "-native")
            if cache.is_symlink():
                raise ValueError("invalid_native_cache")
            cache.mkdir(mode=0o700, exist_ok=True)
            info = cache.stat()
            if info.st_uid != os.geteuid() or info.st_mode & 0o077:
                raise ValueError("invalid_native_cache")
            fingerprint = hashlib.sha256(json.dumps([candidate["version"], checksums], sort_keys=True).encode()).hexdigest()
            destination = cache / fingerprint
            if destination.is_symlink():
                raise ValueError("invalid_native_cache")
            if not destination.exists():
                with tempfile.TemporaryDirectory(prefix=".native-", dir=cache) as temporary:
                    prepared = Path(temporary) / "release"
                    prepared.mkdir(mode=0o700)
                    for name, relative in NATIVE_FILES.items():
                        path = prepared / relative
                        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                        with sources[name].open("rb") as source, path.open("xb") as target:
                            shutil.copyfileobj(source, target, 1024 * 1024)
                        path.chmod(0o700)
                        if native_hash(path, private=True) != checksums[name]:
                            raise ValueError("native_payload_changed")
                    os.rename(prepared, destination)
            files = {str(destination / relative): checksums[name] for name, relative in NATIVE_FILES.items()}
            if any(native_hash(Path(name), private=True) != checksum for name, checksum in files.items()):
                raise ValueError("native_payload_changed")
            stage = "version"
            native = destination / "bin/codex"
            environment = {key: os.environ[key] for key in ["PATH", "LANG"] if key in os.environ}
            version_probe = run([str(native), "--version"], env=environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=10)
            if version_probe.returncode or version_probe.stdout.decode().strip() != candidate["version"]:
                raise ValueError("native_version_mismatch")
            native_expected = dict(native_codex=str(native), native_version=candidate["version"], native_sha256=checksums["codex"], files=files)
            stage = "prepare"
            if json.loads((root / "settings.json").read_text()) != settings:
                raise ValueError("installation_changed")
            with tempfile.TemporaryDirectory(prefix=".native-stage-", dir=root) as temporary:
                arguments = setup_arguments(settings)[1:]
                arguments[arguments.index("--native-codex") + 1] = str(native)
                arguments += ["--prepare-only", temporary]
                if settings.get("launch_prefix"):
                    arguments += ["--launch-prefix", *settings["launch_prefix"]]
                install_environment = os.environ.copy()
                install_environment["AI_FENCE_DESKTOP_CLI_SOURCE"] = str(root / "releases" / settings["release"] / "ai-fence-cli")
                prepared = run([sys.executable, "-B", str(script), "control", *arguments],
                               env=install_environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=45)
                if prepared.returncode:
                    raise ValueError("native_setup_rejected")
                stage = "activation"
                activate(root, settings, Path(temporary), native_expected=native_expected)
            result.update(status="installed", native_version=candidate["version"])
    except CandidateError as error:
        result.update(status="failed", failure_stage="bootstrap", classification=error.report["classification"])
    except ValueError as error:
        allowed = {"windows_desktop_unavailable", "invalid_native_version", "desktop_runtime_downgrade",
                   "invalid_native_payload", "native_payload_changed", "invalid_native_cache",
                   "native_version_mismatch", "installation_changed", "native_setup_rejected"}
        result.update(status="failed", failure_stage=stage, classification=str(error) if str(error) in allowed else "invalid_candidate")
    except (OSError, KeyError, TypeError, subprocess.SubprocessError):
        result.update(status="failed", failure_stage=stage)
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)
        try:
            save(root / "native-update-status.json", result)
        except OSError:
            pass
    return result


def check(settings, force=False, check_only=False, opener=urlopen, run=subprocess.run):
    if not force and settings.get("update_policy", "off") != "auto":
        return dict(status="disabled")
    if platform.system() != "Linux" or platform.machine().lower() not in {"x86_64", "amd64"}:
        return dict(status="unsupported_target", supported_target="linux-x86_64")
    root = Path(settings["install_root"])
    receipt = root / "update-status.json"
    try:
        descriptor = os.open(root / ".update.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    except OSError:
        return dict(status="failed", failure_stage="update_lock")
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return dict(status="busy")
        try:
            previous = json.loads(receipt.read_text()) if receipt.exists() else {}
        except (OSError, ValueError):
            previous = {}
        now = time.time()
        next_check = previous.get("next_check", 0)
        if not force and isinstance(next_check, (int, float)) and next_check > now:
            return dict(status="deferred")
        stage = "manifest"
        result = dict(status="failed", checked_at=now, next_check=now + 300)
        try:
            # If a client disconnects or kills startup during download, the
            # next connection immediately uses the known working release.
            save(receipt, dict(result, status="checking"))
        except OSError:
            return dict(status="failed", failure_stage="update_status")
        try:
            base = (settings.get("update_base_url") or default_base_url(settings.get("channel"))).rstrip("/")
            origin = safe_url(base)
            if origin.query:
                raise ValueError("release base cannot contain query parameters")
            with opener(Request(base + "/downloads/ai-fence-cli-manifest.json?target=linux-x86_64",
                                headers={"User-Agent": "AI-Fence-Desktop-Updater"}), timeout=3) as response:
                manifest = json.loads(response.read(65537))
            approved = manifest["version"]
            version(approved)
            selected_channel = channel(settings.get("channel"))
            asset = safe_url(manifest["download_url"])
            github = (asset.hostname == "github.com" and asset.path ==
                      f"/Yilnic/ai-fence-cli/releases/download/v{approved}/ai-fence-cli-linux-x86_64")
            same_origin = (asset.scheme, asset.netloc) == (origin.scheme, origin.netloc)
            if (manifest.get("target") != "linux-x86_64" or manifest.get("filename") != "ai-fence-cli-linux-x86_64"
                    or not (github or same_origin) or not re.fullmatch(r"[0-9a-f]{64}", manifest["sha256"])
                    or type(manifest["size_bytes"]) is not int or not 0 < manifest["size_bytes"] <= 512 * 1024 * 1024):
                raise ValueError("invalid release manifest")
            expected_name = "ai-fence-cli" + ("" if selected_channel == "prod" else "-" + selected_channel)
            if (manifest.get("cli_name", "ai-fence-cli") != expected_name
                    or (selected_channel != "prod" and not same_origin)):
                raise ValueError("release manifest does not match installation channel")
            installed_version = version(settings.get("cli_version", "unknown"))
            same_version_current = (selected_channel == "prod" or manifest["sha256"] == settings.get("cli_sha256"))
            if version(approved) < installed_version or (version(approved) == installed_version and same_version_current):
                result.update(status="current")
            elif check_only:
                result.update(status="available", available_version=approved)
            else:
                stage = "download"
                directory = root / "downloads" / manifest["sha256"]
                directory.mkdir(parents=True, exist_ok=True, mode=0o700)
                binary = directory / "ai-fence-cli"
                if binary.is_symlink():
                    raise ValueError("cached release cannot be a symlink")
                if not binary.exists():
                    fd, temporary = tempfile.mkstemp(prefix=".download-", dir=directory)
                    try:
                        with os.fdopen(fd, "wb") as output, opener(Request(manifest["download_url"],
                                headers={"User-Agent": "AI-Fence-Desktop-Updater"}), timeout=3) as response:
                            deadline, size = time.monotonic() + 30, 0
                            while chunk := response.read(1024 * 1024):
                                size += len(chunk)
                                if size > manifest["size_bytes"] or time.monotonic() > deadline:
                                    raise ValueError("release download exceeded its bounds")
                                output.write(chunk)
                            output.flush()
                            os.fsync(output.fileno())
                        with open(temporary, "rb") as stream:
                            if size != manifest["size_bytes"] or hashlib.file_digest(stream, "sha256").hexdigest() != manifest["sha256"]:
                                raise ValueError("release checksum mismatch")
                        os.replace(temporary, binary)
                    finally:
                        if os.path.exists(temporary):
                            os.unlink(temporary)
                stage = "checksum"
                with binary.open("rb") as stream:
                    actual = hashlib.file_digest(stream, "sha256").hexdigest()
                if binary.stat().st_size != manifest["size_bytes"] or actual != manifest["sha256"]:
                    raise ValueError("release checksum mismatch")
                binary.chmod(0o700)
                # Never expose runtime/provider/MCP credentials to release probes.
                environment = {key: os.environ[key] for key in ["PATH", "HOME", "LANG"] if key in os.environ}
                stage = "version"
                probe = run([str(binary), "--version"], env=environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=10)
                if probe.returncode or probe.stdout.decode().strip() != "ai-fence-cli " + approved:
                    raise ValueError("release version mismatch")
                current = json.loads((root / "settings.json").read_text())
                if current != settings:
                    raise ValueError("installation changed during update")
                stage = "install"
                with tempfile.TemporaryDirectory(prefix=".update-stage-", dir=root) as temporary:
                    staging = Path(temporary)
                    prefix_args = ["--launch-prefix", *settings["launch_prefix"]] if settings.get("launch_prefix") else []
                    installed = run([str(binary), *setup_arguments(settings), "--prepare-only", str(staging), *prefix_args],
                                    env=environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=45)
                    if installed.returncode:
                        raise ValueError("official setup rejected update preparation")
                    prepared = json.loads((staging / "settings.json").read_text())
                    if prepared.get("cli_version") != approved or prepared.get("cli_sha256") != actual:
                        raise ValueError("prepared release version/hash mismatch")
                    stage = "activation"
                    activate(root, settings, staging)
                observed = json.loads((root / "settings.json").read_text())
                if observed.get("cli_version") != approved or observed.get("cli_sha256") != actual:
                    raise ValueError("installed release verification failed")
                result.update(status="installed", installed_version=approved, installed_sha256=actual)
            if result["status"] in {"current", "installed"}:
                result["next_check"] = now + settings.get("update_interval_seconds", 86400)
        except CandidateError as error:
            result.update(status="failed", failure_stage="bootstrap", bootstrap=error.report)
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
            result.update(status="failed", failure_stage=stage)
        try:
            save(receipt, result)
        except OSError:
            pass
        return result
    finally:
        os.close(descriptor)
