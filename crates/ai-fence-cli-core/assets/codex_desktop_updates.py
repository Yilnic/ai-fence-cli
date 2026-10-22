"""Launch-bound immutable CLI updates; no service, timer or active restart."""
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePath
import platform
import re
import subprocess
import tempfile
import time
from urllib.parse import urlsplit
from urllib.request import Request, urlopen


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
    return result


def activate(root, expected, staging):
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
        for field in ["install_root", "state_home", "state_identity", "profile", "auth_home", "auth_identity", "auth_mode", "auth_account_key",
                      "native_codex", "native_version", "native_sha256", "roots", "snapshot_policy", "snapshot_directory", "workspace_registry", "update_policy"]:
            if proposed.get(field) != current.get(field):
                raise ValueError("automatic update cannot change profile or filesystem policy")
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
        save(root / "previous-settings.json", current)
        save(root / "settings.json", proposed)
    finally:
        os.close(descriptor)


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
                probe = run([str(binary), "--version"], env=environment, capture_output=True, timeout=10)
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
                                    env=environment, capture_output=True, timeout=45)
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
        except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError):
            result.update(status="failed", failure_stage=stage)
        try:
            save(receipt, result)
        except OSError:
            pass
        return result
    finally:
        os.close(descriptor)
