#!/usr/bin/env python3
"""AI Fence native Codex integration. Python 3.11+, Linux/WSL, stdio JSON-RPC.

Only metadata is persisted here. Codex owns authentication, SQLite and rollouts.
The installer never launches an inference request or edits native history.
"""
import sys
if sys.platform != "linux" or sys.version_info < (3, 11):
    print("ai-fence codex: native desktop setup/runtime requires Linux/WSL and Python 3.11+", file=sys.stderr)
    raise SystemExit(2)

import argparse
import contextlib
import fcntl
import hashlib
import grp
import pwd
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import tomllib
import threading
import time
import uuid
from urllib.parse import urlsplit
import importlib.util

_auth_spec = importlib.util.spec_from_file_location("ai_fence_codex_auth", Path(__file__).with_name("codex_desktop_auth.py"))
auth_runtime = importlib.util.module_from_spec(_auth_spec)
_auth_spec.loader.exec_module(auth_runtime)
auth_policy = auth_runtime.policy
_update_spec = importlib.util.spec_from_file_location("ai_fence_codex_updates", Path(__file__).with_name("codex_desktop_updates.py"))
updates = importlib.util.module_from_spec(_update_spec)
_update_spec.loader.exec_module(updates)

WINDOWS_SESSION_LAUNCHER = r'''[CmdletBinding()]
param(
    [ValidateSet('Fenced','Native')][string]$Mode = 'Fenced',
    [string]$LinuxAdapterDirectory,
    [string]$Executable,
    [switch]$CheckOnly
)
$ErrorActionPreference = 'Stop'
if (-not $Executable) {
    $package = Get-AppxPackage -Name 'OpenAI.Codex' | Sort-Object Version -Descending | Select-Object -First 1
    if (-not $package) { throw 'Specify -Executable for a non-Store Codex installation.' }
    $Executable = Join-Path $package.InstallLocation 'app\Codex.exe'
}
if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { throw 'Codex executable not found.' }
if ($Mode -eq 'Fenced' -and -not $LinuxAdapterDirectory.StartsWith('/')) { throw 'Supply the installed Linux adapter bin directory.' }
$running = @(Get-Process -Name Codex,ChatGPT -ErrorAction SilentlyContinue | Where-Object {
    $_.Path -and [IO.Path]::GetFullPath($_.Path) -eq [IO.Path]::GetFullPath($Executable)
})
$previous = @{}
foreach ($name in @('AI_FENCE_CODEX_DESKTOP','CODEX_CLI_PATH','CODEX_INSTALL_DIR','WSLENV')) {
    $previous[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    $env:CODEX_CLI_PATH = $null
    $env:CODEX_INSTALL_DIR = $null
    $env:AI_FENCE_CODEX_DESKTOP = '0'
    $entries = @($env:WSLENV -split ':' | Where-Object {
        $_ -and ($_ -split '/',2)[0] -notin @('AI_FENCE_CODEX_DESKTOP','CODEX_CLI_PATH','CODEX_INSTALL_DIR')
    })
    $entries += 'AI_FENCE_CODEX_DESKTOP/u'
    if ($Mode -eq 'Fenced') {
        $env:AI_FENCE_CODEX_DESKTOP = '1'
        $env:CODEX_INSTALL_DIR = $LinuxAdapterDirectory
        $entries += 'CODEX_INSTALL_DIR/u'
    }
    $env:WSLENV = $entries -join ':'
    if ($CheckOnly) {
        [pscustomobject]@{Executable=$Executable; Mode=$Mode; RequiresAppExit=($running.Count -gt 0);
            LinuxAdapterDirectory=$LinuxAdapterDirectory; AppStarted=$false; PersistentSettingsChanged=$false}
        return
    }
    if ($running.Count -gt 0) { throw 'An existing app cannot inherit this environment. Quit it when its sessions are ready, then launch this session.' }
    Start-Process -FilePath $Executable -WindowStyle Normal
} finally {
    foreach ($name in $previous.Keys) { [Environment]::SetEnvironmentVariable($name,$previous[$name],'Process') }
}
'''


FORMAT = 1
MARKER = "# AI_FENCE_CODEX_DESKTOP=1"
OFF = {"0", "off", "false", "no"}


def fail(message):
    raise RuntimeError(message)


def digest(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def atomic(path, value, mode=0o600):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, temporary = tempfile.mkstemp(prefix=".af-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(value if isinstance(value, bytes) else value.encode())
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def save(path, value):
    atomic(path, json.dumps(value, indent=2) + "\n")


def load(path):
    with Path(path).open() as stream:
        value = json.load(stream)
    if value.get("format") != FORMAT:
        fail("unsupported metadata version; preserve this installation and upgrade the helper")
    return value


@contextlib.contextmanager
def lock(path, blocking=True, shared=False):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(fd, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX) | (0 if blocking else fcntl.LOCK_NB))
        except BlockingIOError:
            fail("metadata lease is busy; an active or starting launch must finish")
        yield fd
    finally:
        os.close(fd)


def trusted(path, executable=False):
    path = Path(path).resolve(strict=True)
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid not in {0, os.geteuid()} or info.st_mode & 0o022:
        fail("payload must be an owned regular file without group/other write access")
    if executable and not os.access(path, os.X_OK):
        fail("payload is not executable")
    for parent in path.parents:
        info = parent.stat()
        mode = info.st_mode
        private_group = (info.st_uid == os.geteuid()
                         and not [name for name in grp.getgrgid(info.st_gid).gr_mem if name != pwd.getpwuid(os.geteuid()).pw_name]
                         and not [user for user in pwd.getpwall() if user.pw_gid == info.st_gid and user.pw_uid != os.geteuid()])
        if not mode & stat.S_ISVTX and (mode & 0o002 or (mode & 0o020 and not private_group)):
            fail("payload parent is writable by another user")
    return path


def profile_name(value):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", value):
        fail("invalid profile name")
    return value


def reconnect_enabled(settings, explicit=None):
    if explicit is not None:
        return explicit
    value = os.environ.get("AI_FENCE_CODEX_RECONNECT", settings.get("reconnect_policy", "auto")).lower()
    if value not in OFF | {"1", "on", "true", "yes", "auto"}:
        fail("AI_FENCE_CODEX_RECONNECT must be auto/on or off")
    return value not in OFF


def invocation_channel():
    return updates.channel(os.environ.get("AI_FENCE_CLI_CHANNEL", os.environ.get("AI_FENCE_STAGE")))


def default_install_root(channel):
    suffix = "" if updates.channel(channel) == "prod" else "-" + updates.channel(channel)
    return Path.home() / (".local/share/ai-fence-codex-desktop" + suffix)


def setup_channel_options(args, old):
    selected = updates.channel(getattr(args, "channel", None) or invocation_channel())
    explicit_url = getattr(args, "update_base_url", None)
    if explicit_url:
        return selected, explicit_url
    if old and updates.channel(old.get("channel")) == selected and old.get("update_base_url"):
        return selected, old["update_base_url"]
    suffix = "" if selected == "prod" else "-" + selected
    root = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    config = root / ("ai-fence" + suffix) / "cli.toml"
    saved = tomllib.loads(config.read_text()) if config.is_file() else {}
    staged = "AI_FENCE_" + re.sub(r"[^A-Z0-9]", "_", selected.upper()) + "_URL"
    base = os.environ.get(staged) if selected != "prod" else None
    base = base or saved.get("fence_url") or updates.default_base_url(selected)
    if not base:
        fail("configure this channel with setup --url or supply --update-base-url before desktop setup")
    return selected, base


def bind_channel(settings, environment):
    environment["AI_FENCE_CLI_CHANNEL"] = updates.channel(settings.get("channel"))


def installation_registry(channel=None):
    selected = updates.channel(channel) if channel is not None else invocation_channel()
    suffix = "" if selected == "prod" else "-" + selected
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / ("ai-fence" + suffix) / "codex-desktop-installs.json"


def register_installation(settings):
    root = Path(settings["install_root"]).resolve(strict=True)
    trusted(root / "settings.json")
    registry = installation_registry(settings.get("channel", "prod"))
    with lock(registry.with_suffix(".lock")):
        record = load(registry) if registry.exists() else dict(format=FORMAT, installations=[])
        entry = dict(install_root=str(root), identity=identity(root), profile=settings["profile"], state_home=settings["state_home"])
        entries = [v for v in record["installations"] if v["install_root"] != str(root)] + [entry]
        entries.sort(key=lambda v: v["install_root"])
        updated = dict(format=FORMAT, installations=entries)
        if updated != record:
            save(registry, updated)


def unregister_installation(settings):
    registry = installation_registry(settings.get("channel", "prod"))
    if registry.exists():
        with lock(registry.with_suffix(".lock")):
            record = load(registry)
            record["installations"] = [v for v in record["installations"] if v["install_root"] != settings["install_root"]]
            save(registry, record)


def reconnect_after_update(settings, result, explicit=None):
    if not reconnect_enabled(settings, explicit):
        return dict(status="disabled")
    accepted = result["status"] in {"installed", "current"}
    if result["status"] == "deferred":
        path = Path(settings["install_root"]) / "update-status.json"
        accepted = path.exists() and json.loads(path.read_text()).get("status") in {"installed", "current"}
    if not accepted:
        return dict(status="not_scheduled", update_status=result["status"])
    selected = load(Path(settings["install_root"]) / "settings.json")
    return reconnect(selected, apply=True, older_only=True, automatic=explicit is not True)


def update_registered(source_base_url):
    """Terminal update hook; registered paths only, no history/home discovery."""
    updates.safe_url(source_base_url)
    registry = installation_registry()
    if not registry.exists():
        return dict(status="none", installations=[])
    with lock(registry.with_suffix(".lock")):
        entries = load(registry)["installations"]
    source = trusted(os.environ["AI_FENCE_DESKTOP_CLI_SOURCE"], True)
    probe = subprocess.run([str(source), "--version"], capture_output=True, timeout=10,
                           env={"PATH": os.environ.get("PATH", "/usr/bin:/bin")})
    source_version = probe.stdout.decode().strip().removeprefix("ai-fence-cli ")
    if probe.returncode:
        fail("cannot identify the terminal CLI release")
    updates.version(source_version)
    results = []
    for entry in entries:
        root = Path(entry["install_root"])
        try:
            if not root.is_dir() or identity(root) != entry["identity"] or not (root / "settings.json").is_file():
                results.append(dict(install_root=str(root), status="missing_or_replaced"))
                continue
            trusted(root / "settings.json")
            settings = load(root / "settings.json")
            if any(settings.get(k) != entry[k] for k in ["install_root", "profile", "state_home"]):
                fail("registered installation identity changed")
            if (updates.channel(settings.get("channel")) != invocation_channel()
                    or settings.get("update_policy", "off") != "auto"
                    or (settings.get("update_base_url") or updates.default_base_url(settings.get("channel"))).rstrip("/") != source_base_url.rstrip("/")):
                results.append(dict(install_root=str(root), status="disabled_or_other_source"))
                continue
            newer = updates.version(source_version) > updates.version(settings.get("cli_version", "unknown"))
            if invocation_channel() != "prod" and updates.version(source_version) == updates.version(settings.get("cli_version", "unknown")):
                newer = digest(source) != settings.get("cli_sha256")
            result = updates.check(settings, force=newer)
            result["reconnect"] = reconnect_after_update(settings, result)
            results.append(dict(install_root=str(root), **result))
        except (OSError, ValueError, RuntimeError, KeyError, TypeError, subprocess.SubprocessError):
            results.append(dict(install_root=str(root), status="failed"))
    outcomes = {v.get("reconnect", {}).get("status") for v in results}
    if "scheduled" in outcomes:
        print("ai-fence codex: updated desktop connections will reconnect when idle; reopen their saved chats", file=sys.stderr)
    if "manual_reconnect_required" in outcomes:
        print("ai-fence codex: reconnect older adapters manually once to enable automatic reconnects", file=sys.stderr)
    if any(v["status"] == "failed" for v in results):
        print("ai-fence codex: an automatic desktop update was deferred; inspect codex-desktop doctor", file=sys.stderr)
    return dict(status="checked", installations=results)


def install(args):
    if sys.platform != "linux" or sys.version_info < (3, 11):
        fail("native desktop setup currently requires Linux/WSL and Python 3.11+")
    root = args.install_root.expanduser().absolute()
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    root = root.resolve()
    if root.stat().st_mode & 0o022:
        fail("install root must not be group/other writable")
    with lock(root / ".install.lock"):
        native = trusted(args.native_codex, True)
        cli = trusted(os.environ["AI_FENCE_DESKTOP_CLI_SOURCE"], True)
        if native.is_relative_to(root) or native == cli:
            fail("native executable must be outside the adapter installation")
        with native.open("rb") as stream:
            header = stream.read(4096)
        if b"AI_FENCE_" in header or b"codex-fenced" in header:
            fail("native executable is an AI Fence wrapper; select the genuine Codex binary")
        result = subprocess.run([str(native), "--version"], capture_output=True, timeout=10)
        version = result.stdout.decode(errors="replace").strip()
        if result.returncode or not re.fullmatch(r"codex-cli [A-Za-z0-9.+_-]+", version):
            fail("native executable did not report a supported codex-cli version")
        native_help = subprocess.run([str(native), "--help"], capture_output=True, timeout=10)
        no_daemon_supported = native_help.returncode == 0 and b"--no-daemon" in native_help.stdout
        settings_path = root / "settings.json"
        old = load(settings_path) if settings_path.exists() else None
        selected_channel, update_base = setup_channel_options(args, old)
        updates.safe_url(update_base)
        prepared = getattr(args, "prepare_only", None)
        destination = Path(prepared).resolve() if prepared else root
        destination.mkdir(parents=True, exist_ok=True, mode=0o700)
        if prepared and (destination == root or any(destination.iterdir())):
            fail("update preparation requires an empty private staging directory")
        if old:
            for name, expected in old["files"].items():
                owned = root / name
                if owned.is_symlink() or not owned.is_file() or digest(owned) != expected:
                    fail("owned installation file changed; preserve and inspect it before upgrading")
        elif (root / "dispatch.py").exists():
            fail("unowned dispatch file exists")
        launch_prefix = getattr(args, "launch_prefix", None)
        if launch_prefix is None:
            launch_prefix = (old or {}).get("launch_prefix", [])
        if launch_prefix:
            trusted(Path(launch_prefix[0]), True)
        for name in ["codex", "codex-runtime", "ai-fence-snapshot"]:
            candidate = root / "bin" / name
            if candidate.is_symlink() or (candidate.exists() and not old):
                fail("unowned adapter command exists; choose an empty install root")
        home = args.state_home.expanduser().resolve()
        if old and (old["state_home"] != str(home) or old["profile"] != args.profile):
            fail("upgrade cannot change profile/state-home identity; use a separate installation")
        if old:
            if not home.is_dir() or identity(home) != old["state_identity"]:
                fail("installed state home is missing or replaced; reattach its filesystem without creating a replacement")
        else:
            home.mkdir(parents=True, exist_ok=True, mode=0o700)
        profile_name(args.profile)
        selected_auth = getattr(args, "auth_home", None) or (old or {}).get("auth_home")
        if selected_auth is None:
            # An upgrade retains its account boundary until explicitly selected.
            selected_auth = home if old else Path(os.environ.get("CODEX_HOME", Path.home() / ".codex"))
        auth = auth_policy(home, Path(selected_auth).expanduser(), expected_key=(old or {}).get("auth_account_key"))
        if old and old.get("auth_account_key") and auth.get("auth_account_key") != old["auth_account_key"]:
            fail("upgrade cannot change the credential account bound to existing history")
        if auth["auth_mode"] == "native-owner":
            with tempfile.TemporaryDirectory(prefix="af-codex-schema-") as schema:
                probe_env = os.environ.copy()
                probe_env["CODEX_HOME"] = schema
                probe = subprocess.run([str(native), "app-server", "generate-json-schema", "--out", schema],
                                       env=probe_env, capture_output=True, timeout=15)
                login_schema = Path(schema) / "v2/LoginAccountParams.json"
                if (probe.returncode or not login_schema.is_file() or
                        '"chatgptAuthTokens"' not in login_schema.read_text() or
                        not (Path(schema) / "ChatgptAuthTokensRefreshResponse.json").exists()):
                    fail("native Codex lacks the external-token login/refresh API; update native Codex or select profile auth")
        if args.snapshots != "off" and args.approve_root and not args.snapshot_directory:
            fail("approved roots require --snapshot-directory on the same Btrfs filesystem")
        files = {}
        runtime = Path(__file__).read_bytes()
        auth_body = Path(__file__).with_name("codex_desktop_auth.py").read_bytes()
        update_body = Path(__file__).with_name("codex_desktop_updates.py").read_bytes()
        cli_probe = subprocess.run([str(cli), "--version"], capture_output=True, timeout=10,
                                   env={"PATH":os.environ.get("PATH", "/usr/bin:/bin")})
        cli_match = re.fullmatch(rb"ai-fence-cli ([0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?)\s*", cli_probe.stdout)
        cli_version = cli_match[1].decode() if cli_probe.returncode == 0 and cli_match else "unknown"
        release_id = hashlib.sha256(runtime + auth_body + update_body + digest(cli).encode()).hexdigest()[:24]
        release = root / "releases" / release_id
        release.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Content-addressed releases retain binaries used by running processes.
        for name, data, mode in [("runtime.py", runtime, 0o700), ("codex_desktop_auth.py", auth_body, 0o600),
                                 ("codex_desktop_updates.py", update_body, 0o600),
                                 ("codex-session.ps1", WINDOWS_SESSION_LAUNCHER.encode(), 0o600)]:
            dest = release / name
            if not dest.exists():
                atomic(dest, data, mode)
            if digest(dest) != hashlib.sha256(data).hexdigest():
                fail("installed release was modified; refusing to overwrite it")
            files[str(dest.relative_to(root))] = digest(dest)
        dest = release / "ai-fence-cli"
        if not dest.exists():
            temporary = release / (".cli-" + uuid.uuid4().hex)
            shutil.copyfile(cli, temporary)
            temporary.chmod(0o700)
            os.replace(temporary, dest)
        if digest(dest) != digest(cli):
            fail("installed CLI was modified")
        files[str(dest.relative_to(root))] = digest(dest)
        release_helper = release / "codex-runtime"
        helper_body = ("#!/bin/sh\n" + MARKER + "\nexec " + shell(sys.executable) + " "
                       + shell(str(release / "runtime.py")) + ' runtime "$@"\n')
        if not release_helper.exists():
            atomic(release_helper, helper_body, 0o700)
        if digest(release_helper) != hashlib.sha256(helper_body.encode()).hexdigest():
            fail("immutable runtime helper changed")
        files[str(release_helper.relative_to(root))] = digest(release_helper)
        for name, mode in [("codex", "adapter"), ("codex-runtime", "runtime"), ("ai-fence-snapshot", "snapshot")]:
            wrapper = root / "bin" / name
            prefix = " ".join(shell(value) for value in launch_prefix)
            body = ("#!/bin/sh\n" + MARKER + "\nexec " + (prefix + " " if prefix else "") + shell(sys.executable) + " "
                    + shell(str(root / "dispatch.py")) + " " + mode + ' "$@"\n')
            if wrapper.exists() and MARKER not in wrapper.read_text():
                fail("unowned adapter command exists; choose an empty install root")
            target_wrapper = destination / wrapper.relative_to(root)
            atomic(target_wrapper, body, 0o700)
            files[str(wrapper.relative_to(root))] = digest(target_wrapper)
        # Stable dispatch selects one immutable release per invocation.
        dispatch = 'import json,os,sys\nfrom pathlib import Path\nr=Path(__file__).resolve().parent\ns=json.loads((r/"settings.json").read_text())\nos.environ["_AI_FENCE_DESKTOP_SETTINGS"]=json.dumps(s)\nos.execv(sys.executable,[sys.executable,str(r/"releases"/s["release"]/"runtime.py"),*sys.argv[1:]])\n'
        if (root / "dispatch.py").exists() and old is None:
            fail("unowned dispatch file exists")
        atomic(destination / "dispatch.py", dispatch, 0o700)
        files["dispatch.py"] = digest(destination / "dispatch.py")
        settings = dict(format=FORMAT, install_root=str(root), release=release_id, channel=selected_channel,
                        native_codex=str(native), native_version=version, native_sha256=digest(native), no_daemon_supported=no_daemon_supported,
                        profile=profile_name(args.profile), state_home=str(home), state_identity=identity(home), readiness_seconds=args.readiness_seconds,
                        workspace_registry=str((getattr(args, "workspace_registry", None) or Path(os.environ.get("AI_FENCE_WORKSPACE_HOME", Path.home() / ".ai-fence/workspace-registry"))).expanduser().absolute()),
                        seed_home=str(home),
                        snapshot_policy=args.snapshots, roots=[str(p.expanduser().absolute()) for p in args.approve_root],
                        snapshot_directory=str(args.snapshot_directory.expanduser().absolute()) if args.snapshot_directory else None,
                        files=files, launch_prefix=launch_prefix, cli_version=cli_version, cli_sha256=digest(cli),
                        update_policy=getattr(args, "updates", None) or (old or {}).get("update_policy", "auto"),
                        reconnect_policy=getattr(args, "reconnect", None) or (old or {}).get("reconnect_policy", "auto"),
                        update_base_url=update_base,
                        update_interval_seconds=getattr(args, "update_interval_seconds", None) or (old or {}).get("update_interval_seconds", 86400), **auth)
        updates.safe_url(settings["update_base_url"])
        if settings["snapshot_policy"] != "off" and settings["roots"] and not settings["snapshot_directory"]:
            fail("approved roots require --snapshot-directory on the same Btrfs filesystem")
        save(destination / "snapshot-roots.json", dict(format=FORMAT, roots=settings["roots"]))
        if old and old != settings and not prepared:
            save(root / "previous-settings.json", old)
        if args.snapshots != "off" and not prepared:
            install_guidance(home)
        # This is the only release switch; existing children retain old settings.
        save(destination / "settings.json", settings)
        if not prepared:
            try:
                register_installation(settings)
            except (OSError, ValueError, RuntimeError):
                print("ai-fence codex: terminal update registration deferred; run codex-desktop register when available", file=sys.stderr)
        print(json.dumps({"installed": True, "adapter": str(root / "bin/codex"),
                          "channel": selected_channel, "update_base_url": update_base,
                          "running_connections_unchanged": True,
                          "profile": settings["profile"], "state_home": str(home),
                          "native_version": version, "release": release_id,
                          "discovery": "select this adapter for the WSL connection; desktop discovery varies by version"}))


def shell(value):
    return "'" + value.replace("'", "'\"'\"'") + "'"


def cli_payload(settings):
    return Path(settings["install_root"]) / "releases" / settings.get("release", "fixture") / "ai-fence-cli"


def named_registry(settings):
    return Path(settings.get("workspace_registry", os.environ.get("AI_FENCE_WORKSPACE_HOME", Path.home() / ".ai-fence/workspace-registry")))


def named_call(settings, action, *arguments, registry=None):
    """Shared public workspace engine; only IDs, paths and lifecycle metadata."""
    env = os.environ.copy()
    env["AI_FENCE_WORKSPACE_HOME"] = str(registry or named_registry(settings))
    env["AI_FENCE_CLI_AUTO_UPDATE"] = "never"
    result = subprocess.run([str(cli_payload(settings)), "workspace", action, *map(str, arguments)],
                            env=env, capture_output=True, timeout=20)
    if result.returncode:
        detail = result.stderr.decode(errors="replace").strip()
        fail("named workspace unavailable: " + (detail[-500:] or "inspect ai-fence-cli workspace status"))
    return json.loads(result.stdout)


def named_context(settings, cwd):
    physical = Path(cwd).resolve(strict=True)
    if not any((root / ".ai-fence-workspace.json").exists() for root in [physical, *physical.parents]) and not (named_registry(settings) / "records").exists():
        return None
    return named_call(settings, "status", "--path", physical)


def same_named_identity(expected, path):
    actual = identity(path)
    if expected.get("subvolume_uuid"):
        return expected["subvolume_uuid"] == actual["subvolume_uuid"] and expected["inode"] == actual["inode"]
    return (expected["device"], expected["inode"]) == (actual["device"], actual["inode"])


def named_resume(settings, parameters):
    thread_id = parameters.get("threadId")
    if not thread_id:
        return False
    path = record_path(settings, thread_id)
    local = load(path) if path.exists() else None
    reference = (local or {}).get("named_workspace")
    registry = Path(reference["registry"]) if reference else named_registry(settings)
    if not (registry / "records").exists():
        if reference:
            fail("saved named workspace registry is missing; explicit recovery is required")
        return False
    saved = named_call(settings, "_resolve", "--thread-id", thread_id,
                       "--profile", settings["profile"], "--state-home", settings["state_home"], registry=registry)
    if not saved:
        if reference:
            fail("thread workspace link is missing from the selected profile/store; inspect before recovery")
        return False
    record, expected = saved["record"], saved["cwd"]
    if parameters.get("cwd"):
        selected = named_context(settings, parameters["cwd"])
        if not selected or selected["id"] != record["id"]:
            fail("resume selected another named workspace; start/fork a thread there explicitly")
    else:
        if not expected or not same_named_identity(expected, expected["path"]):
            fail("saved thread directory is missing/replaced; explicit recovery is required")
        parameters["cwd"] = expected["path"]
    return True


def install_guidance(home):
    path = Path(home) / "AGENTS.md"
    begin, end = "<!-- ai-fence-desktop-checkpoints:start -->", "<!-- ai-fence-desktop-checkpoints:end -->"
    if path.is_symlink():
        fail("native AGENTS.md is a symlink; preserve its instructions and configure checkpoint guidance explicitly")
    content = path.read_text() if path.exists() else ""
    guidance = (begin + "\nWorkspace checkpoints are read-only and retain the original cwd. Select the current project explicitly from AI_FENCE_SNAPSHOT_ROOTS_FILE; process startup IDs may belong to another chat. Use ai-fence-snapshot status/create/list --root EXACT_APPROVED_ROOT --directory STORE, with --recursive for nested subvolumes. Quiesce writers for consistency. Checkpoints do not isolate Git metadata, restore files, or automatically align with conversation turns. Never restore on ordinary resume or delete active, dirty, pinned or chat-linked workspaces.\n" + end)
    if begin in content:
        start, stop = content.index(begin), content.index(end) + len(end)
        content = content[:start] + guidance + content[stop:]
    else:
        content = content.rstrip() + "\n\n" + guidance + "\n"
    atomic(path, content)


def wait_for_payloads(settings):
    deadline = time.monotonic() + settings.get("readiness_seconds", 0)
    while True:
        try:
            if (Path(settings["native_codex"]).is_file() and
                    identity(settings["state_home"]) == settings.get("state_identity", identity(settings["state_home"]))):
                return
        except OSError:
            pass
        if time.monotonic() >= deadline:
            fail("native payload/state filesystem is unavailable or changed; retry after storage is attached, no replacement home was created")
        time.sleep(0.1)


def seed_auth(settings):
    home = Path(settings["state_home"])
    source = Path(settings["seed_home"]) / "auth.json"
    target = home / "auth.json"
    if source == target or not source.is_file():
        return
    with lock(home / ".ai-fence-auth-seed.lock"):
        if target.exists():
            return
        # Publish a complete bundle atomically, only if native login has not
        # already installed one. A killed seed cannot leave a partial auth file.
        fd, temporary = tempfile.mkstemp(prefix=".af-auth-seed-", dir=home)
        try:
            with source.open("rb") as src, os.fdopen(fd, "wb") as dst:
                os.fchmod(dst.fileno(), 0o600)
                shutil.copyfileobj(src, dst)
                dst.flush()
                os.fsync(dst.fileno())
            try:
                os.link(temporary, target)
            except FileExistsError:
                pass
        finally:
            os.unlink(temporary)


def settings_for_runtime():
    # runtime.py is under root/releases/<content-id>/.
    if os.environ.get("_AI_FENCE_DESKTOP_SETTINGS"):
        return json.loads(os.environ["_AI_FENCE_DESKTOP_SETTINGS"])
    return load(Path(__file__).resolve().parents[2] / "settings.json")


def endpoint(value):
    parsed = urlsplit(value)
    if (parsed.scheme != "http" or parsed.hostname != "127.0.0.1" or not parsed.port
            or parsed.path != "/v1" or parsed.username or parsed.password or parsed.query or parsed.fragment):
        fail("expected owning AI Fence process's loopback /v1 endpoint")
    return parsed


def subcommand(arguments):
    values = {"-c", "--config", "-p", "--profile", "-m", "--model", "-s", "--sandbox",
              "-a", "--ask-for-approval", "-C", "--cd", "--enable", "--disable", "--add-dir", "-i", "--image"}
    skip = False
    for i, argument in enumerate(arguments):
        if skip:
            skip = False
        elif argument in values:
            skip = True
        elif argument == "--":
            return (arguments[i + 1], i + 1) if i + 1 < len(arguments) else (None, None)
        elif not argument.startswith("-"):
            return argument, i
    return None, None


def native_cwd(arguments):
    directory = Path.cwd()
    index = 0
    while index < len(arguments):
        value = arguments[index]
        if value == "--":
            break
        if value in {"-C", "--cd"}:
            index += 1
            if index == len(arguments):
                fail("Codex -C/--cd needs a directory")
            directory = Path.cwd() / arguments[index]
        elif value.startswith("--cd="):
            directory = Path.cwd() / value[5:]
        elif value.startswith("-C") and len(value) > 2:
            directory = Path.cwd() / value[2:]
        index += 1
    return directory


def runtime(settings, arguments):
    value = os.environ.get("OPENAI_BASE_URL", "")
    endpoint(value)
    command, position = subcommand(arguments)
    # Insert after existing globals, before command / argument separator.
    index = position if position is not None else len(arguments)
    if index and arguments[index - 1] == "--":
        index -= 1
    if command != "app-server" and settings.get("no_daemon_supported") and "--no-daemon" not in arguments:
        arguments.insert(index, "--no-daemon")
    arguments[index:index] = ["-c", 'model_provider="ai_fence"', "-c",
                             "model_providers.ai_fence=" + '{name="AI Fence", wire_api="responses", requires_openai_auth=true, base_url=' + json.dumps(value) + '}']
    if settings.get("auth_mode") == "native-owner":
        arguments[index:index] = ["-c", 'cli_auth_credentials_store="ephemeral"']
    env = os.environ.copy()
    bind_channel(settings, env)
    env["AI_FENCE_SNAPSHOTS"] = env.pop("_AI_FENCE_DESKTOP_CHECKPOINT_POLICY", settings["snapshot_policy"])
    for key in ["AI_FENCE_SNAPSHOT_ROOT", "AI_FENCE_SNAPSHOT_ID", "AI_FENCE_SNAPSHOT_PATH", "AI_FENCE_SNAPSHOT_RECEIPT"]:
        env.pop(key, None)
    env["AI_FENCE_SNAPSHOT_STATUS"] = "thread-scoped"
    env["AI_FENCE_SNAPSHOT_ROOTS_FILE"] = str(Path(settings["install_root"]) / "snapshot-roots.json")
    if settings["snapshot_directory"]:
        env["AI_FENCE_SNAPSHOT_DIR"] = settings["snapshot_directory"]
    # Publish only sanitized per-launch route ownership, never process environments.
    if env.get("_AI_FENCE_DESKTOP_LAUNCH"):
        receipt = Path(env.pop("_AI_FENCE_DESKTOP_LAUNCH"))
        with lock(receipt.with_suffix(".lock")):
            item = load(receipt)
            item.update(endpoint=value, proxy_pid=os.getppid(), proxy_start=process_identity(os.getppid()),
                        app_server_pid=os.getpid())
            save(receipt, item)
    env.pop("_AI_FENCE_DESKTOP_OWNED", None)
    env.pop("_AI_FENCE_DESKTOP_SETTINGS", None)
    os.execve(settings["native_codex"], [settings["native_codex"], *arguments], env)


def process_identity(pid):
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
        return Path("/proc/sys/kernel/random/boot_id").read_text().strip() + ":" + text[text.rfind(")") + 2:].split()[19]
    except (OSError, IndexError):
        return None


def alive(lease):
    return bool(lease.get("start") and process_identity(lease["pid"]) == lease["start"])


def identity(path):
    path = Path(path).resolve(strict=True)
    info = path.stat()
    subvolume = None
    try:
        buffer = bytearray(504)
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
        try:
            fcntl.ioctl(fd, 0x81F8943C, buffer, True)
        finally:
            os.close(fd)
        subvolume = str(uuid.UUID(bytes=bytes(buffer[296:312])))
    except (OSError, ValueError):
        pass
    return {"physical": str(path), "device": info.st_dev, "inode": info.st_ino, "subvolume_uuid": subvolume}


def record_path(settings, thread_id):
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", thread_id):
        fail("invalid thread identity")
    return Path(settings["state_home"]) / "ai-fence-workspaces" / (thread_id + ".json")


def attach(settings, thread_id, cwd, lease, checkpoint=None, mode="checkpoint", named=None):
    path = record_path(settings, thread_id)
    with lock(path.with_suffix(".lock")):
        project = identity(cwd)
        if path.exists():
            record = load(path)
            if not Path(record["workspace"]["physical"]).exists():
                fail("saved workspace is missing; recover to a new workspace from a chosen checkpoint or explicitly adopt a replacement")
            if record["workspace"] != project and not (named and record.get("named_workspace", {}).get("id") == named["id"]):
                fail("resume requested another workspace; explicit adoption/recovery is required")
            if record["profile"] != settings["profile"] or record["state_home"] != settings["state_home"]:
                fail("profile/state-home identity mismatch")
        else:
            record = dict(format=FORMAT, thread_id=thread_id, profile=settings["profile"],
                          state_home=settings["state_home"], project=project, logical_root=str(cwd),
                          workspace=project, mode=mode, checkpoints=[], leases=[], pinned=False)
        if any(alive(item) and item != lease for item in record["leases"]):
            fail("thread workspace has an active lease in another process")
        record["leases"] = [item for item in record["leases"] if alive(item) and item != lease]
        record["leases"].append(lease)
        if checkpoint and checkpoint not in record["checkpoints"]:
            record["checkpoints"].append(checkpoint)
        if named:
            record["mode"] = "named"
            record["workspace"] = project
            record["logical_root"] = str(cwd)
            source = named["project"]
            record["project"] = dict(physical=source["path"], device=source["device"], inode=source["inode"], subvolume_uuid=source["subvolume_uuid"])
            record["named_workspace"] = {key: named[key] for key in ["id", "name", "registry"]}
        save(path, record)
        return record


def checkpoint(settings, cwd, thread_id):
    if os.environ.get("AI_FENCE_SNAPSHOTS", settings["snapshot_policy"]) == "off":
        return None
    # Select the current approved project, never the app-server's startup root.
    current = Path(cwd).resolve(strict=True)
    roots = [p for p in settings["roots"] if Path(p).resolve() == current]
    if not roots:
        if os.environ.get("AI_FENCE_SNAPSHOTS", settings["snapshot_policy"]) == "required":
            fail("required checkpoint needs the current project in the explicit approved-root registry")
        return None
    env = os.environ.copy()
    env["AI_FENCE_SNAPSHOTS"] = os.environ.get("AI_FENCE_SNAPSHOTS", settings["snapshot_policy"])
    env.pop("AI_FENCE_SNAPSHOT_ROOT", None)
    policy = Path(settings["install_root"]) / "snapshot-roots.json"
    env["AI_FENCE_SNAPSHOT_ROOTS_FILE"] = str(policy)
    cli = cli_payload(settings)
    args = [str(cli), "snapshot", "create", "--root", roots[0], "--directory",
            settings["snapshot_directory"], "--recursive"]
    result = subprocess.run(args, env=env, capture_output=True, timeout=60)
    if result.returncode:
        if env.get("AI_FENCE_SNAPSHOTS", settings["snapshot_policy"]) == "required":
            fail("required current-project checkpoint failed; inspect snapshot status")
        return None
    receipt = json.loads(result.stdout)
    # Pin before registering a durable chat reference; retained blocks use space.
    if receipt.get("id"):
        subprocess.run([str(cli), "snapshot", "pin", "--root", roots[0], "--directory",
                        settings["snapshot_directory"], receipt["id"]],
                       env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10, check=True)
    return {"id": receipt.get("id"), "path": receipt.get("path"), "thread_id": thread_id}


def preflight_resume(settings, parameters):
    thread_id = parameters.get("threadId")
    if not thread_id:
        return
    if named_resume(settings, parameters):
        path = record_path(settings, thread_id)
        if path.exists():
            record = load(path)
            if any(alive(item) and item["pid"] != os.getpid() for item in record["leases"]):
                fail("thread already has an active workspace lease")
        return
    path = record_path(settings, thread_id)
    if path.exists():
        record = load(path)
        current = Path(record["workspace"]["physical"])
        if any(alive(item) and item["pid"] != os.getpid() for item in record["leases"]):
            fail("thread already has an active workspace lease")
        if not current.exists():
            fail("saved workspace is missing; use workspace recovery/adoption explicitly, no fallback performed")
        if identity(current) != record["workspace"]:
            fail("saved workspace physical identity changed; explicit recovery is required")
        if parameters.get("cwd") and identity(parameters["cwd"]) != record["workspace"]:
            fail("resume cwd differs from the saved workspace; explicit adoption is required")
        # Retain the caller's alias, or the original checkpoint-mode logical cwd.
        # An isolated/adopted workspace uses its saved physical destination.
        if not parameters.get("cwd"):
            logical = Path(record.get("logical_root", current))
            if record["mode"] == "checkpoint" and logical.exists() and identity(logical) == record["workspace"]:
                parameters["cwd"] = str(logical)
            else:
                parameters["cwd"] = str(current)
    elif not parameters.get("cwd") and settings.get("native_codex"):
        # A CLI-created thread may not yet have a Fence registration. Query
        # native metadata, without turns, to find its existing named workspace.
        thread = native_thread(settings, thread_id)
        if thread and thread.get("cwd") and named_context(settings, thread["cwd"]):
            parameters["cwd"] = thread["cwd"]


class Lifecycle:
    """Observe the actual protocol; never persist inputs, outputs or transcripts."""
    def __init__(self, settings, lease):
        self.settings, self.lease = settings, lease
        self.pending = {}
        self.gate = threading.RLock()
        self.threads = set()
        self.named = {}
        self.named_threads = {}
        self.startup = set()
        self.child_pid = None
        self.opt_out = set()

    def reserve(self, cwd, thread_id=None):
        if not named_context(self.settings, cwd):
            return None
        args = ["--path", cwd, "--pid", self.lease["pid"]]
        if self.child_pid:
            args.extend(["--child-pid", self.child_pid])
        inherited = os.environ.get("AI_FENCE_WORKSPACE_LEASE")
        if inherited:
            args.extend(["--inherited", inherited])
        if thread_id:
            args.extend(["--thread-id", thread_id, "--profile", self.settings["profile"],
                         "--state-home", self.settings["state_home"]])
        value = named_call(self.settings, "_attach", *args)
        if value:
            with self.gate:
                self.named.setdefault(value["id"], value)
                if thread_id:
                    self.named_threads[thread_id] = value["id"]
        return value

    def release_unused(self, workspace_id):
        with self.gate:
            if workspace_id in self.startup or workspace_id in self.named_threads.values():
                return
            if any(pending.get("workspace") == workspace_id for pending in self.pending.values()):
                return
            value = self.named.pop(workspace_id, None)
        if value and value["owned"]:
            named_call(self.settings, "_release", "--id", value["id"], "--lease", value["lease"],
                       "--pid", self.lease["pid"], registry=value["registry"])

    def closed_thread(self, thread_id):
        with self.gate:
            workspace_id = self.named_threads.pop(thread_id, None)
            self.threads.discard(thread_id)
        if thread_id:
            path = record_path(self.settings, thread_id)
            if path.exists():
                with lock(path.with_suffix(".lock")):
                    record = load(path)
                    record["leases"] = [item for item in record["leases"] if item != self.lease and alive(item)]
                    save(path, record)
        if workspace_id:
            self.release_unused(workspace_id)

    def close(self):
        with self.gate:
            self.pending.clear()
            self.startup.clear()
            threads = list(self.threads)
        for thread_id in threads:
            self.closed_thread(thread_id)
        for workspace_id in list(self.named):
            self.release_unused(workspace_id)

    def request(self, value):
        method = value.get("method")
        parameters = value.get("params") or {}
        if method == "initialize":
            capabilities = parameters.get("capabilities") or {}
            if isinstance(capabilities, dict):
                requested = capabilities.get("optOutNotificationMethods")
                if isinstance(requested, list):
                    self.opt_out = {item for item in requested if isinstance(item, str)}
                    observed = {"thread/started", "thread/closed", "thread/status/changed",
                                "turn/started", "turn/completed"}
                    capabilities["optOutNotificationMethods"] = [item for item in requested if item not in observed]
        resumed = method in {"thread/resume", "turn/start", "thread/shellCommand"}
        if resumed:
            preflight_resume(self.settings, parameters)
        if method == "thread/fork" and not parameters.get("cwd") and self.settings.get("native_codex"):
            parent = native_thread(self.settings, parameters.get("threadId"))
            if parent and parent.get("cwd"):
                parameters["cwd"] = parent["cwd"]
        reservation = None
        if method in {"thread/start", "thread/resume", "thread/fork", "turn/start", "thread/shellCommand"}:
            cwd = parameters.get("cwd")
            if not cwd and method == "thread/start":
                cwd = os.getcwd()
            if cwd:
                reservation = self.reserve(cwd)
        if resumed:
            thread_id = parameters.get("threadId")
            path = record_path(self.settings, thread_id) if thread_id else None
            try:
                if path and path.exists():
                    with lock(path.with_suffix(".lock")):
                        preflight_resume(self.settings, parameters)
                        record = load(path)
                        record["leases"] = [v for v in record["leases"] if alive(v) and v != self.lease]
                        record["leases"].append(self.lease)
                        save(path, record)
            except BaseException:
                if reservation:
                    self.release_unused(reservation["id"])
                raise
        if method in {"thread/start", "thread/resume", "thread/fork", "thread/unsubscribe", "turn/start", "thread/shellCommand"} and "id" in value:
            with self.gate:
                self.pending[str(value["id"])] = dict(method=method, thread_id=parameters.get("threadId"),
                                                     workspace=reservation["id"] if reservation else None)

    def response(self, value):
        with self.gate:
            pending = self.pending.pop(str(value.get("id")), None)
        thread = (value.get("result") or {}).get("thread") if pending else None
        if value.get("method") == "thread/started":
            thread = value.get("params", {}).get("thread")
        if isinstance(thread, dict) and thread.get("id") and thread.get("cwd"):
            thread_id, cwd = thread["id"], thread["cwd"]
            named = self.reserve(cwd, thread_id)
            receipt = checkpoint(self.settings, cwd, thread_id) if thread_id not in self.threads else None
            attach(self.settings, thread_id, cwd, self.lease, receipt, named=named)
            self.threads.add(thread_id)
        method, parameters = value.get("method"), value.get("params", {})
        if method == "thread/closed" or (method == "thread/status/changed" and parameters.get("status", {}).get("type") == "notLoaded"):
            self.closed_thread(parameters.get("threadId"))
        if pending:
            if "error" in value and pending["thread_id"] and pending["thread_id"] not in self.threads:
                self.closed_thread(pending["thread_id"])
            if not thread and "error" not in value and pending["method"] in {"turn/start", "thread/shellCommand"} and pending["workspace"]:
                self.named_threads[pending["thread_id"]] = pending["workspace"]
                self.threads.add(pending["thread_id"])
            if pending["method"] == "thread/unsubscribe" and value.get("result", {}).get("status") == "notLoaded":
                self.closed_thread(pending["thread_id"])
            if pending["workspace"]:
                self.release_unused(pending["workspace"])
        return value.get("method") not in self.opt_out


def settings_fingerprint(settings):
    return hashlib.sha256(json.dumps(settings, sort_keys=True).encode()).hexdigest()


def reconnect_report(root, operation_id):
    if not isinstance(operation_id, str) or not re.fullmatch(r"[0-9a-f]{32}", operation_id):
        fail("invalid reconnect operation identity")
    return root / "reconnects" / (operation_id + ".json")


def verify_reconnect_install(settings):
    root = Path(settings["install_root"])
    for name, expected in settings["files"].items():
        path = Path(name)
        if path.is_absolute() or ".." in path.parts or (root / path).is_symlink() or digest(root / path) != expected:
            fail("installed release integrity failed; reconnect was not requested")
    if settings.get("native_sha256") and digest(settings["native_codex"]) != settings["native_sha256"]:
        fail("native Codex changed; rerun setup before reconnecting")


def reconnect(settings, apply=False, status=False, cancel=False, pid=None, wait_seconds=60, older_only=False, automatic=False):
    """Arm one atomic, installation-scoped request; each adapter stops itself.

    No PID signals, timers, worker processes, credentials or native RPC injection.
    The existing adapter loop owns the idle boundary and shutdown lifecycle.
    """
    root = Path(settings["install_root"])
    request_path = root / "reconnect-request.json"
    with lock(root / ".reconnect.lock", blocking=False):
        previous = load(request_path) if request_path.exists() else None
        if status or cancel:
            if not previous:
                return dict(status="none", targets=[], reopen_thread_ids=[])
            report_path = reconnect_report(root, previous["operation_id"])
            if cancel:
                previous["cancelled"] = True
                save(request_path, previous)
            with lock(report_path.with_suffix(".lock")):
                report = load(report_path)
                for target in report["targets"]:
                    if target["status"] == "pending":
                        if previous.get("cancelled"):
                            target["status"] = "cancelled"
                        elif previous["expires_at"] <= time.time():
                            target["status"] = "expired"
                        elif not alive(target):
                            target["status"] = "disconnected"
                if cancel:
                    save(report_path, report)
            report["reopen_thread_ids"] = sorted({thread for target in report["targets"] for thread in target.get("threads", [])})
            report["reopen_chats"] = "Reopen saved chats in Codex; restoring desktop tabs is controlled by the desktop client."
            states = {target["status"] for target in report["targets"]}
            report["status"] = "closed" if states == {"closed"} else "pending" if "pending" in states else "shutting_down" if "shutting_down" in states else "finished"
            report["replacement_launches"] = []
            for path in (root / "launches").glob("*.json"):
                item = load(path)
                if (not alive(item) or item.get("mode") != "desktop-stdio" or not item.get("fenced")
                        or item.get("started_at", 0) < previous["created_at"]
                        or item.get("release") != report["selected_release"]
                        or item.get("profile") != report["profile"] or item.get("state_home") != report["state_home"]):
                    continue
                proxy = item.get("proxy_pid")
                try:
                    verified = (process_identity(proxy) == item.get("proxy_start")
                                and digest(Path(f"/proc/{proxy}/exe")) == report["selected_cli_sha256"]
                                and listener_owned(proxy, endpoint(item["endpoint"]).port))
                except (OSError, RuntimeError, ValueError, KeyError):
                    verified = False
                report["replacement_launches"].append(dict(pid=item["pid"], proxy_pid=proxy,
                                                          cli_version=item.get("cli_version"), verified=verified))
            return report
        targets = []
        for path in sorted((root / "launches").glob("*.json")):
            item = load(path)
            if (not alive(item) or item.get("mode") != "desktop-stdio" or not item.get("fenced")
                    or item.get("profile") != settings["profile"] or item.get("state_home") != settings["state_home"]
                    or (pid is not None and item["pid"] != pid)
                    or (older_only and item.get("release") == settings["release"])):
                continue
            targets.append(dict(pid=item["pid"], start=item["start"], release=item.get("release"),
                                status="pending" if item.get("reconnect_control") == 1 else "manual_reconnect_required",
                                threads=item.get("reconnect_state", {}).get("threads", []),
                                activity=item.get("reconnect_state", {})))
        plan = dict(format=FORMAT, status="preview", profile=settings["profile"], state_home=settings["state_home"],
                    selected_release=settings["release"], selected_cli_sha256=settings.get("cli_sha256"), targets=targets)
        if not apply or not targets:
            return plan
        if any(item["status"] == "manual_reconnect_required" for item in targets):
            return dict(plan, status="manual_reconnect_required",
                        action="Reconnect older adapters manually once to enable cooperative reconnects; no process was signalled.")
        if previous and not previous.get("cancelled") and previous["expires_at"] > time.time():
            report = load(reconnect_report(root, previous["operation_id"]))
            pending = [item for item in report["targets"] if item["status"] in {"pending", "shutting_down"} and alive(item)]
            if pending:
                if (previous["settings_fingerprint"] == settings_fingerprint(settings)
                        and {(v["pid"], v["start"]) for v in previous["targets"]} == {(v["pid"], v["start"]) for v in targets}):
                    return report
                fail("a reconnect is already pending; inspect reconnect --status or cancel it first")
        if not 1 <= wait_seconds <= 300:
            fail("reconnect wait must be between 1 and 300 seconds")
        with lock(root / ".install.lock", blocking=False):
            if load(root / "settings.json") != settings:
                fail("installation changed; preview reconnect again")
            verify_reconnect_install(settings)
            operation_id = uuid.uuid4().hex
            request = dict(plan, status="scheduled", operation_id=operation_id, automatic=automatic,
                           settings_fingerprint=settings_fingerprint(settings), created_at=time.time(), expires_at=time.time() + wait_seconds)
            save(reconnect_report(root, operation_id), request)
            # This sole atomic replacement arms the entire selected set. A new
            # launch cannot be swept up by a process-name or endpoint search.
            save(request_path, request)
            return request


class ReconnectSession:
    """Observe protocol metadata and serialize an idle shutdown against input."""
    def __init__(self, settings, live, lifecycle):
        self.settings, self.live, self.lifecycle = settings, live, lifecycle
        self.root, self.lease = Path(settings["install_root"]), load(live)
        self.gate = threading.RLock()
        self.pending, self.approvals, self.active, self.completed = {}, set(), set(), set()
        self.inputs = 0
        self.ready, self.draining = False, False
        self.operation, self.terminal, self.reopen_threads = None, set(), []
        self.published, self.error = None, None
        self.publish()

    def publish(self):
        summary = dict(ready=self.ready, pending_requests=len(self.pending), pending_approvals=len(self.approvals),
                       processing_inputs=self.inputs,
                       active_turns=len(self.active | {thread for thread in self.pending.values() if thread}),
                       threads=sorted(self.lifecycle.threads), draining=self.draining, error=self.error)
        if summary != self.published:
            with lock(self.live.with_suffix(".lock")):
                item = load(self.live)
                item.update(reconnect_control=1, reconnect_state=summary)
                save(self.live, item)
            self.published = summary

    @contextlib.contextmanager
    def input(self):
        # Do not hold this gate while auth synchronously awaits native output.
        # Count the entire input operation instead, including preflight/refresh.
        with self.gate:
            allowed = not self.draining
            if allowed:
                self.inputs += 1
                self.publish()
        try:
            yield allowed
        finally:
            if allowed:
                with self.gate:
                    self.inputs -= 1
                    self.publish()

    def client(self, value):
        if self.draining:
            fail("connection is reconnecting; reopen the saved chat after reconnect")
        method = value.get("method")
        key = json.dumps(value.get("id"))
        if method and "id" in value:
            self.pending[key] = (value.get("params") or {}).get("threadId") if method == "turn/start" else None
        elif not method and "id" in value:
            self.approvals.discard(key)
        if method == "initialized":
            self.ready = True
        self.publish()

    def native(self, value):
        method, params = value.get("method"), value.get("params") or {}
        key = json.dumps(value.get("id"))
        if method and "id" in value:
            self.approvals.add(key)
        elif not method and "id" in value:
            thread = self.pending.pop(key, None)
            turn = (value.get("result") or {}).get("turn") or {}
            if thread and "error" not in value and turn.get("id") not in self.completed and turn.get("status") not in {"completed", "failed", "interrupted"}:
                self.active.add(thread)
        if method == "turn/started":
            self.active.add(params.get("threadId"))
        elif method == "turn/completed":
            self.active.discard(params.get("threadId"))
            self.completed.add((params.get("turn") or {}).get("id"))
            if len(self.completed) > 256:
                self.completed = {(params.get("turn") or {}).get("id")}
        elif method in {"thread/closed", "thread/status/changed"}:
            if method == "thread/closed" or params.get("status", {}).get("type") in {"notLoaded", "idle"}:
                self.active.discard(params.get("threadId"))
        self.publish()

    def mark(self, status):
        path = reconnect_report(self.root, self.operation)
        with lock(path.with_suffix(".lock")):
            report = load(path)
            target = next(v for v in report["targets"] if v["pid"] == self.lease["pid"] and v["start"] == self.lease["start"])
            target.update(status=status, threads=self.reopen_threads if self.draining else sorted(self.lifecycle.threads))
            save(path, report)

    def poll(self):
        try:
            return self.poll_checked()
        except (OSError, ValueError, RuntimeError, KeyError, TypeError, StopIteration):
            # Malformed/stale local control metadata cannot take down a chat.
            with self.gate:
                self.draining = False
                self.error = "reconnect_request_rejected"
                try:
                    self.publish()
                except (OSError, ValueError, RuntimeError):
                    pass
            return False

    def poll_checked(self):
        with self.gate:
            path = self.root / "reconnect-request.json"
            if self.draining or not path.exists():
                return False
            request = load(path)
            if request["operation_id"] in self.terminal:
                return False
            if not any(v["pid"] == self.lease["pid"] and v["start"] == self.lease["start"] for v in request["targets"]):
                return False
            self.operation = request["operation_id"]
            reason = ("cancelled" if request.get("cancelled") else "expired" if request["expires_at"] <= time.time()
                      else "disabled" if request.get("automatic") and not reconnect_enabled(load(self.root / "settings.json"))
                      else "installation_changed" if settings_fingerprint(load(self.root / "settings.json")) != request["settings_fingerprint"] else None)
            if reason:
                self.mark(reason)
                self.terminal.add(self.operation)
                return False
            if not self.ready or self.inputs or self.pending or self.approvals or self.active:
                return False
            # Check again under the same lock order as the command. Cancelling
            # or activating another release cannot race the shutdown boundary.
            with lock(self.root / ".reconnect.lock", blocking=False), lock(self.root / ".install.lock", blocking=False):
                current = load(path)
                if (current != request or request["expires_at"] <= time.time()
                        or settings_fingerprint(load(self.root / "settings.json")) != request["settings_fingerprint"]):
                    return False
                verify_reconnect_install(load(self.root / "settings.json"))
                self.reopen_threads = sorted(self.lifecycle.threads)
                self.draining = True
                self.mark("shutting_down")
                self.publish()
                return True

    def finish(self):
        if self.draining:
            self.mark("closed")


def adapter(settings, arguments):
    if os.environ.get("_AI_FENCE_DESKTOP_OWNED") == "1":
        fail("recursive desktop adapter dispatch; native executable/runtime must be absolute")
    wait_for_payloads(settings)
    env = os.environ.copy()
    bind_channel(settings, env)
    env.pop("CODEX_INSTALL_DIR", None)
    env.pop("CODEX_CLI_PATH", None)
    command, position = subcommand(arguments)
    tail = arguments[position + 1:] if position is not None else []
    bypass = env.get("AI_FENCE_CODEX_DESKTOP", "1").lower() in OFF
    discovery = any(x in {"--version", "-V", "--help", "-h"} for x in arguments)
    native_commands = {"login", "logout", "completion", "migrate-rollouts", "features", "debug", "help"}
    shared_auth = settings.get("auth_mode") == "native-owner"
    if shared_auth and command != "app-server" and command not in native_commands and not discovery:
        fail("shared native auth requires the desktop stdio app-server; use ordinary native Codex for TUI or select the profile as --auth-home")
    # Native routing still needs the external-token bridge when history and
    # credentials have separate owners. Direct exec would reuse stale profile auth.
    direct_native = bypass and shared_auth and command == "app-server"
    if (bypass and not direct_native) or discovery or command in native_commands or (tail and tail[0] in {"generate-ts", "generate-json-schema"}):
        # Keep CLI entry points in the selected history store as well.
        env["CODEX_HOME"] = settings.get("auth_home", settings["state_home"]) if command in {"login", "logout"} else settings["state_home"]
        if bypass:
            arguments = ["-c", 'model_provider="openai"', *arguments]
        os.execve(settings["native_codex"], [settings["native_codex"], *arguments], env)
    if tail and tail[0] == "daemon":
        if tail[1:] == ["version"]:
            os.execve(settings["native_codex"], [settings["native_codex"], *arguments], env)
        fail("background daemon cannot be supervised; select a stdio WSL app-server")
    if any(x == "--listen" or x.startswith("--listen=") for x in tail):
        fail("only the desktop stdio transport is supported by this adapter")
    root = Path(settings["install_root"])
    env["CODEX_HOME"] = settings["seed_home"]
    env["PATH"] = str(root / "bin") + os.pathsep + env.get("PATH", "")
    # Startup cwd is not necessarily any thread's approved project.
    env["_AI_FENCE_DESKTOP_CHECKPOINT_POLICY"] = env.get("AI_FENCE_SNAPSHOTS", settings["snapshot_policy"])
    env["_AI_FENCE_DESKTOP_SETTINGS"] = json.dumps(settings)
    env["_AI_FENCE_DESKTOP_OWNED"] = "1"
    env["AI_FENCE_SNAPSHOTS"] = "off"
    env["AI_FENCE_CLI_AUTO_UPDATE"] = "never"
    env["AI_FENCE_WORKSPACE_HOME"] = str(named_registry(settings))
    cli = cli_payload(settings)
    if settings.get("auth_mode") != "native-owner":
        seed_auth(settings)
    argv = [str(cli), "run", "--no-codex-config", "--standalone", "--localsubscription", "--agent", "codex",
            "--profile", settings["profile"], "--codex-home", settings["state_home"],
            "--codex-auth-source", "managed", "--", str(cli.parent / "codex-runtime"), *arguments]
    if direct_native:
        argv = [settings["native_codex"], "-c", 'model_provider="openai"',
                "-c", 'cli_auth_credentials_store="ephemeral"', *arguments]
    lease = dict(pid=os.getpid(), start=process_identity(os.getpid()))
    live = root / "launches" / (str(os.getpid()) + ".json")
    save(live, dict(format=FORMAT, **lease, started_at=time.time(), child_pid=None, profile=settings["profile"],
                    state_home=settings["state_home"], logical_cwd=os.environ.get("PWD", os.getcwd()),
                    physical_cwd=str(Path.cwd().resolve()), fenced=not direct_native,
                    mode="native-stdio" if direct_native else "desktop-stdio" if command == "app-server" else "cli",
                    snapshot_policy=env["_AI_FENCE_DESKTOP_CHECKPOINT_POLICY"], endpoint=None, release=settings["release"],
                    cli_version=settings.get("cli_version", "unknown"), cli_sha256=settings.get("cli_sha256")))
    env["_AI_FENCE_DESKTOP_LAUNCH"] = str(live)
    interactive = command != "app-server"
    lifecycle = Lifecycle(settings, lease)
    if interactive and command == "resume" and tail and re.fullmatch(r"[a-fA-F0-9-]{36}", tail[0]):
        parameters = dict(threadId=tail[0])
        preflight_resume(settings, parameters)
        if parameters.get("cwd"):
            argv = argv[:argv.index("--") + 2] + ["-C", parameters["cwd"], *arguments]
            named = lifecycle.reserve(parameters["cwd"], tail[0])
            attach(settings, tail[0], parameters["cwd"], lease, named=named)
    if interactive:
        cwd = parameters["cwd"] if command == "resume" and tail and re.fullmatch(r"[a-fA-F0-9-]{36}", tail[0]) and parameters.get("cwd") else native_cwd(arguments)
        selected = lifecycle.reserve(cwd)
        if selected:
            lifecycle.startup.add(selected["id"])
            env["AI_FENCE_WORKSPACE_LEASE"] = selected["lease"]
            env["AI_FENCE_WORKSPACE_HOME"] = selected["registry"]
    try:
        child = subprocess.Popen(argv, env=env, stdin=None if interactive else subprocess.PIPE,
                                 stdout=None if interactive else subprocess.PIPE,
                                 start_new_session=True, bufsize=0)
    except BaseException:
        lifecycle.close()
        live.unlink(missing_ok=True)
        raise
    lifecycle.child_pid = child.pid
    with lock(live.with_suffix(".lock")):
        item = load(live)
        item["child_pid"] = child.pid
        save(live, item)
    reconnection = ReconnectSession(settings, live, lifecycle) if not interactive and not direct_native else None
    output_lock = threading.Lock()
    def send(value):
        with output_lock:
            sys.stdout.buffer.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
            sys.stdout.buffer.flush()
    native_lock = threading.Lock()
    def native_send(value):
        with native_lock:
            child.stdin.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
            child.stdin.flush()
    auth_bridge = auth_runtime.Bridge(settings, native_send, send) if settings.get("auth_mode") == "native-owner" else None
    def input_loop():
        reader = selectors.DefaultSelector()
        reader.register(sys.stdin.fileno(), selectors.EVENT_READ)
        pending = b""
        try:
            while child.poll() is None:
                if not reader.select(0.2):
                    continue
                chunk = os.read(sys.stdin.fileno(), 65536)
                if not chunk:
                    break
                pending += chunk
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    value = json.loads(line)
                    with reconnection.input() if reconnection else contextlib.nullcontext(True) as allowed:
                        if not allowed:
                            if "id" in value and "method" in value:
                                send({"jsonrpc": "2.0", "id": value["id"], "error": {
                                    "code": -32000, "message": "AI Fence connection is reconnecting; reopen the saved chat.",
                                    "data": {"origin": "ai_fence", "stage": "reconnect", "retryable": True, "native_forwarded": False}}})
                            continue
                        try:
                            if auth_bridge:
                                if value.get("method") == "initialize":
                                    auth_runtime.initialize(settings, value)
                                if auth_bridge.client(value):
                                    continue
                                if value.get("method") == "turn/start" and not auth_bridge.sync():
                                    raise auth_runtime.AuthError()
                            lifecycle.request(value)
                        except auth_runtime.AuthError as error:
                            send(auth_bridge.error(value.get("id"), error))
                            continue
                        except (RuntimeError, OSError, ValueError) as error:
                            send({"jsonrpc": "2.0", "id": value.get("id"), "error": {
                                "code": -32602, "message": "AI Fence workspace preflight: " + str(error)[:500],
                                "data": {"origin":"ai_fence", "stage":"workspace_preflight", "retryable":False, "native_forwarded":False}}})
                            continue
                        with reconnection.gate if reconnection else contextlib.nullcontext():
                            if reconnection:
                                reconnection.client(value)
                            native_send(value)
                        if auth_bridge and value.get("method") == "initialized":
                            try:
                                auth_bridge.sync()
                            except auth_runtime.AuthError as error:
                                send(auth_bridge.error(None, error))
        except (BrokenPipeError, OSError, ValueError):
            pass
        finally:
            reader.close()
            child.stdin.close()
    if not interactive:
        threading.Thread(target=input_loop, daemon=True).start()
    deadline = None
    def stop(signum, _frame):
        nonlocal deadline
        deadline = time.monotonic() + 5
        try:
            os.killpg(child.pid, signum)
        except ProcessLookupError:
            pass
    for signum in [signal.SIGTERM, signal.SIGINT, signal.SIGHUP]:
        signal.signal(signum, stop)
    if interactive:
        try:
            while child.poll() is None:
                if deadline and time.monotonic() > deadline:
                    os.killpg(child.pid, signal.SIGKILL)
                time.sleep(0.1)
            return child.returncode if child.returncode >= 0 else 128 - child.returncode
        finally:
            try:
                os.killpg(child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            lifecycle.close()
            live.unlink(missing_ok=True)
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    buffer = b""
    try:
        while True:
            if reconnection and reconnection.poll():
                stop(signal.SIGTERM, None)
            if deadline and time.monotonic() > deadline:
                os.killpg(child.pid, signal.SIGKILL)
                deadline = None
            ready = selector.select(0.2)
            if not ready:
                if child.poll() is not None:
                    break
                continue
            block = os.read(child.stdout.fileno(), 65536)
            if not block:
                break
            buffer += block
            while b"\n" in buffer:
                line, buffer = buffer.split(b"\n", 1)
                value = json.loads(line)
                if auth_bridge and auth_bridge.runtime(value):
                    continue
                try:
                    with reconnection.gate if reconnection else contextlib.nullcontext():
                        forward = lifecycle.response(value)
                        if reconnection:
                            reconnection.native(value)
                        if forward:
                            send(value)
                except (RuntimeError, OSError, ValueError, subprocess.SubprocessError):
                    print("ai-fence codex: thread workspace registration/checkpoint failed", file=sys.stderr)
                    stop(signal.SIGTERM, None)
                    break
        status = child.wait(timeout=5)
    finally:
        if auth_bridge:
            auth_bridge.close()
        selector.close()
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
        lifecycle.close()
        if reconnection:
            try:
                reconnection.finish()
            except (OSError, ValueError, RuntimeError, KeyError, StopIteration):
                print("ai-fence codex: reconnect receipt unavailable after shutdown", file=sys.stderr)
        live.unlink(missing_ok=True)
    return status if status >= 0 else 128 - status


def doctor(settings, args):
    root = Path(settings["install_root"])
    launches = []
    for path in sorted((root / "launches").glob("*.json")):
        item = load(path)
        item["alive"] = alive(item)
        if item.get("endpoint"):
            target = endpoint(item["endpoint"])
            item["proxy_identity_alive"] = process_identity(item.get("proxy_pid")) == item.get("proxy_start")
            item["listener_owned"] = listener_owned(item.get("proxy_pid"), target.port)
            try:
                with socket.create_connection((target.hostname, target.port), timeout=1):
                    item["endpoint_tcp_alive"] = True
            except OSError:
                item["endpoint_tcp_alive"] = False
        launches.append(item)
    files = {name: (root / name).is_file() and digest(root / name) == sha
             for name, sha in settings["files"].items()}
    home = Path(settings["state_home"])
    native_config = {}
    if (home / "config.toml").exists():
        with (home / "config.toml").open("rb") as stream:
            native_config = tomllib.load(stream)
    sqlite_home = native_config.get("sqlite_home", os.environ.get("CODEX_SQLITE_HOME", str(home)))
    named = named_context(settings, Path.cwd())
    report = dict(format=FORMAT, profile=settings["profile"], state_home=str(home),
                  channel=updates.channel(settings.get("channel")), update_base_url=settings.get("update_base_url") or updates.default_base_url(settings.get("channel")),
                  auth_home=settings.get("auth_home", str(home)), auth_mode=settings.get("auth_mode", "profile"),
                  credential_probe="not performed; transport/history availability does not prove provider authentication",
                  installed_cli_version=settings.get("cli_version", "unknown"), installed_cli_sha256=settings.get("cli_sha256"),
                  update_policy=settings.get("update_policy", "off"),
                  reconnect_policy=settings.get("reconnect_policy", "auto"),
                  automatic_reconnect_enabled=reconnect_enabled(settings),
                  running_older_release=any(item["alive"] and item.get("release") != settings["release"] for item in launches),
                  native_path=settings["native_codex"], native_version=settings["native_version"],
                  native_unchanged=Path(settings["native_codex"]).is_file() and digest(settings["native_codex"]) == settings["native_sha256"],
                  owned_files=files, launches=launches, logical_cwd=os.environ.get("PWD", os.getcwd()),
                  physical_cwd=str(Path.cwd().resolve()), mode="named" if named else "original",
                  snapshot_policy=os.environ.get("AI_FENCE_SNAPSHOTS", settings["snapshot_policy"]),
                  workspace_registry=str(named_registry(settings)), named_workspace=named,
                  sessions_present=(home / "sessions").is_dir(),
                  resume="resume --all removes cwd filtering only within this CODEX_HOME; provider/account filters are separate",
                  sqlite_home=str(Path(sqlite_home).expanduser().resolve()),
                  saved_provider=native_config.get("model_provider", "openai"),
                  effective_fenced_provider="ai_fence (process override)",
                  active_native_configuration="per-launch overrides; owning endpoint is recorded separately")
    update_receipt = root / "update-status.json"
    if update_receipt.exists():
        report["update_status"] = json.loads(update_receipt.read_text())
    if (root / "reconnect-request.json").exists():
        report["reconnect"] = reconnect(settings, status=True)
    if getattr(args, "check_auth", False):
        version = settings.get("native_version", "codex-cli 0").removeprefix("codex-cli ")
        if not re.fullmatch(r"[0-9.]+", version):
            fail("cannot identify native client version for the provider check")
        auth_settings = dict(settings, auth_home=settings.get("auth_home", str(home)))
        report["provider_authentication"] = auth_runtime.provider_check(auth_settings, version)
    if args.endpoint:
        target = endpoint(args.endpoint)
        try:
            with socket.create_connection((target.hostname, target.port), timeout=1):
                report["endpoint_tcp_alive"] = True
        except OSError:
            report["endpoint_tcp_alive"] = False
        report["endpoint"] = args.endpoint
        report["endpoint_owner_verified"] = False
    if args.thread_id:
        path = record_path(settings, args.thread_id)
        report["thread_registration"] = load(path) if path.exists() else None
        report["native_thread"] = native_thread(settings, args.thread_id)
        report["absence_checks"] = ["selected CODEX_HOME", "provider/account selection", "cwd/physical alias filter", "archived status"]
        reference = (report["thread_registration"] or {}).get("named_workspace")
        registry = Path(reference["registry"]) if reference else named_registry(settings)
        if (registry / "records").exists():
            try:
                report["named_thread_workspace"] = named_call(settings, "_resolve", "--thread-id", args.thread_id,
                    "--profile", settings["profile"], "--state-home", settings["state_home"], registry=registry)
            except (RuntimeError, OSError, ValueError):
                report["named_thread_workspace_error"] = "saved named workspace is missing, removed or changed; recover explicitly"
    report["healthy"] = all(files.values()) and report["native_unchanged"] and all(
        not v["alive"] or v.get("mode") == "native-stdio" and v.get("fenced") is False
        or (v.get("listener_owned", False) and v.get("endpoint_tcp_alive", False)) for v in launches)
    if args.endpoint:
        report["healthy"] = report["healthy"] and report["endpoint_tcp_alive"]
    if "provider_authentication" in report:
        report["healthy"] = report["healthy"] and report["provider_authentication"]["authenticated"]
    if args.thread_id:
        if report.get("named_thread_workspace_error"):
            report["healthy"] = False
        thread = report.get("native_thread")
        report["resume_diagnosis"] = []
        if thread is None:
            report["resume_diagnosis"].append("thread is not indexed in the selected CODEX_HOME; --all cannot cross state stores")
        else:
            report["resume_diagnosis"].append("thread is indexed in the selected state store")
            if thread.get("cwd"):
                try:
                    matches = identity(thread["cwd"]) == identity(Path.cwd())
                except OSError:
                    matches = False
                report["cwd_filter_matches"] = matches
                if not matches:
                    report["resume_diagnosis"].append("current cwd differs or is missing; native resume --all removes only this filter")
            if thread.get("modelProvider") and thread["modelProvider"] != "ai_fence":
                report["resume_diagnosis"].append("saved provider differs from the fenced provider; native picker filtering depends on installed version")
            if thread.get("archived"):
                report["resume_diagnosis"].append("thread is archived")
    report["native_protocol_check"] = "use the isolated initialize/config-read integration test; status never attaches to a live stdio stream"
    print(json.dumps(report, indent=2))
    return report["healthy"]


def uninstall(settings, apply):
    root = Path(settings["install_root"])
    with lock(root / ".install.lock"), lock(root / ".lifecycle.lock", blocking=False):
        if any(alive(load(p)) for p in (root / "launches").glob("*.json")):
            fail("active launches protect this installation; close only its sessions before uninstall")
        paths = [root / name for name in settings["files"] if not name.startswith("releases/")]
        if any(p.is_symlink() or not p.is_file() or digest(p) != settings["files"][str(p.relative_to(root))] for p in paths):
            fail("owned files changed; preserve them and inspect before uninstall")
        print(json.dumps({"apply": apply, "remove": [str(p) for p in paths],
                          "preserved": [settings["state_home"], settings["native_codex"], "all releases and workspace records"]}))
        if apply:
            for path in paths:
                path.unlink()
            (root / "settings.json").rename(root / "uninstalled-settings.json")
            try:
                unregister_installation(settings)
            except (OSError, ValueError, RuntimeError):
                print("ai-fence codex: terminal update registration removal deferred", file=sys.stderr)


def listener_owned(pid, port):
    """Match loopback listener socket inode to the recorded proxy PID, no argv/env."""
    if not pid:
        return False
    try:
        inodes = set()
        for line in Path("/proc/net/tcp").read_text().splitlines()[1:]:
            fields = line.split()
            if fields[1] == f"0100007F:{port:04X}" and fields[3] == "0A":
                inodes.add(fields[9])
        return any(os.readlink(fd) in {f"socket:[{inode}]" for inode in inodes}
                   for fd in Path(f"/proc/{pid}/fd").iterdir())
    except OSError:
        return False


def native_thread(settings, thread_id):
    """Use native metadata APIs; never read/merge SQLite or rewrite rollouts."""
    env = os.environ.copy()
    env["CODEX_HOME"] = settings["state_home"]
    env.pop("CODEX_INSTALL_DIR", None)
    env.pop("CODEX_CLI_PATH", None)
    child = subprocess.Popen([settings["native_codex"], "app-server"], env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, start_new_session=True, bufsize=0)
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    pending = b""
    def request(identifier, method, params):
        nonlocal pending
        child.stdin.write(json.dumps(dict(id=identifier, method=method, params=params)).encode() + b"\n")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if not selector.select(0.1):
                continue
            chunk = os.read(child.stdout.fileno(), 65536)
            if not chunk:
                break
            pending += chunk
            while b"\n" in pending:
                line, pending = pending.split(b"\n", 1)
                value = json.loads(line)
                if value.get("id") == identifier:
                    return value
        fail("native app-server metadata query timed out; check installed-version support")
    try:
        initial = request(1, "initialize", dict(clientInfo=dict(name="ai_fence_doctor", version="1")))
        if "error" in initial:
            fail("native app-server initialization rejected; check installed-version support")
        child.stdin.write(b'{"method":"initialized","params":{}}\n')
        response = request(2, "thread/read", dict(threadId=thread_id, includeTurns=False))
        thread = response.get("result", {}).get("thread")
        if thread:
            return {key: thread.get(key) for key in ["id", "cwd", "modelProvider", "archived"]}
        if response.get("error", {}).get("code") in {-32600, -32601}:
            fail("native thread/read rejected the ID or is unsupported; check thread identity and installed-version support")
        return None
    finally:
        selector.close()
        child.stdin.close()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
        child.stdout.close()


def workspace_control(settings, args):
    """The desktop entry point uses the same named workspace CLI as shells."""
    env = os.environ.copy()
    env["AI_FENCE_WORKSPACE_HOME"] = str(named_registry(settings))
    env["AI_FENCE_CLI_AUTO_UPDATE"] = "never"
    return subprocess.call([str(cli_payload(settings)), "workspace", *args.arguments], env=env)


def discovery_router(settings, prefix):
    """Optional default-command router; no desktop-version-specific signature."""
    if prefix:
        trusted(Path(prefix[0]), True)
    import inspect
    parser = inspect.getsource(subcommand)
    return ("#!/usr/bin/env python3\n# AI_FENCE_CODEX_DESKTOP_ROUTER=1\nimport os,sys,json\nfrom pathlib import Path\n"
            + parser + "\nroot=Path(" + repr(settings["install_root"]) + ")\n"
            + "settings=json.loads((root/'settings.json').read_text())\n"
            + "arguments=sys.argv[1:]\ncommand,_=subcommand(arguments)\n"
            + "target=str(root/'bin/codex') if command=='app-server' else settings['native_codex']\n"
            + "launch=" + repr(list(prefix)) + "+[target]+arguments\nos.execv(launch[0],launch)\n")


def control(arguments):
    parser = argparse.ArgumentParser(prog="ai-fence-cli codex-desktop")
    parser.add_argument("--install-root", type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    setup = commands.add_parser("setup", help="install an opt-in desktop adapter without changing native Codex")
    setup.add_argument("--channel", help="installation channel; normally selected by the channel-specific CLI command")
    setup.add_argument("--native-codex", required=True, type=Path)
    setup.add_argument("--state-home", required=True, type=Path, help="explicit stable Codex state store; never a run directory")
    setup.add_argument("--readiness-seconds", type=int, choices=range(0, 121), default=45, help="bounded passive wait for configured native/state storage")
    setup.add_argument("--auth-home", "--seed-home", dest="auth_home", type=Path,
                       help="single native ChatGPT credential owner; defaults to native CODEX_HOME on a fresh setup, retained on upgrade; select --state-home's path for profile auth")
    setup.add_argument("--updates", choices=["auto", "manual", "off"], help="verified checks from terminal updates and future app-server launches (default auto)")
    setup.add_argument("--reconnect", choices=["auto", "off"], help="idle reconnects after verified updates (default auto); retained on upgrade")
    setup.add_argument("--update-base-url", help="trusted HTTPS AI Fence release server; defaults to ai.yaaf.de (production) or ai-dev.yaaf.de (DEV)")
    setup.add_argument("--update-interval-seconds", type=int, choices=range(60, 604801), help="successful update-check interval, default 86400; failures back off 300 seconds")
    setup.add_argument("--prepare-only", type=Path, help=argparse.SUPPRESS)
    setup.add_argument("--launch-prefix", nargs=argparse.REMAINDER,
                       help="existing memory/admission launcher and arguments; supply LAST; retained on upgrades and applied before adapter startup")
    setup.add_argument("--profile", default=os.environ.get("AI_FENCE_CODEX_PROFILE", os.environ.get("AI_FENCE_PROFILE", "default")))
    setup.add_argument("--snapshots", choices=["auto", "off", "required"], default="off")
    setup.add_argument("--workspace-registry", type=Path, help="shared named workspace metadata; defaults to AI_FENCE_WORKSPACE_HOME or ~/.ai-fence/workspace-registry")
    setup.add_argument("--approve-root", type=Path, action="append", default=[])
    setup.add_argument("--snapshot-directory", type=Path)
    switch = commands.add_parser("switch-channel", help="switch an explicitly selected adapter to this CLI channel, retaining history/auth/admission; reconnect when ready")
    switch.add_argument("--update-base-url", help="explicit trusted release source; otherwise use this channel's saved CLI configuration")
    status = commands.add_parser("doctor", aliases=["status"])
    status.add_argument("--strict", action="store_true", help="exit nonzero on unhealthy file/route metadata")
    status.add_argument("--endpoint")
    status.add_argument("--thread-id")
    status.add_argument("--check-auth", action="store_true", help="explicit read-only ChatGPT models request; no refresh, inference, or response content is logged")
    remove = commands.add_parser("uninstall")
    remove.add_argument("--apply", action="store_true")
    workspace = commands.add_parser("workspace", help="shared named workspace commands", add_help=False)
    workspace.add_argument("arguments", nargs=argparse.REMAINDER)
    commands.add_parser("rollback", help="select previous installed release; does not restore history or workspaces")
    commands.add_parser("windows-launcher", help="print the packaged session-only Windows launcher")
    commands.add_parser("register", help="register an existing official installation for terminal automatic updates")
    registered = commands.add_parser("update-registered", help=argparse.SUPPRESS)
    registered.add_argument("--source-base-url", required=True)
    update = commands.add_parser("update", help="install a verified release; optionally reconnect older desktop connections at idle")
    update.add_argument("--check-only", action="store_true")
    reconnect_option = update.add_mutually_exclusive_group()
    reconnect_option.add_argument("--reconnect", action="store_true", default=None, help="enable idle reconnects after update (default on; overrides environment and stored policy)")
    reconnect_option.add_argument("--no-reconnect", dest="reconnect", action="store_false", help="update the installation while retaining existing connections")
    connection = commands.add_parser("reconnect", help="preview or schedule cooperative desktop reconnects without restarting WSL or the desktop app")
    action = connection.add_mutually_exclusive_group()
    action.add_argument("--apply", action="store_true", help="close the selected connections when idle so Codex can reconnect")
    action.add_argument("--status", action="store_true", help="show the last operation and saved thread IDs to reopen")
    action.add_argument("--cancel", action="store_true", help="cancel pending requests; shutdowns already acknowledged cannot be undone")
    connection.add_argument("--pid", type=int, help="select one adapter from doctor; default all managed desktop connections in this installation")
    connection.add_argument("--wait-seconds", type=int, choices=range(1, 301), default=60, metavar="1..300",
                            help="expire an unacknowledged reconnect after this idle wait (default 60); never interrupt active turns")
    router = commands.add_parser("discovery-router", help="print an optional router: all app-server invocations use this profile; ordinary native CLI remains native")
    router.add_argument("--launch-prefix", nargs=argparse.REMAINDER, default=[],
                        help="optional existing memory/admission launcher prefix, applied to BOTH routes")
    # argparse treats a literal -- specially even inside an optional REMAINDER.
    # A launcher prefix is LAST and must retain that delimiter byte-for-byte.
    args = None
    if "--launch-prefix" in arguments:
        marker = arguments.index("--launch-prefix")
        selected = parser.parse_args(arguments[:marker])
        if selected.command in {"setup", "discovery-router"}:
            selected.launch_prefix = arguments[marker + 1:]
            args = selected
    if args is None:
        args = parser.parse_args(arguments)
    if args.command == "switch-channel":
        if args.install_root is None:
            parser.error("switch-channel requires --install-root for the existing adapter")
        current = load(args.install_root.expanduser().resolve() / "settings.json")
        selected = invocation_channel()
        source_args = argparse.Namespace(channel=selected, update_base_url=args.update_base_url)
        _, base = setup_channel_options(source_args, current)
        setup_args = updates.setup_arguments(current)[1:]
        setup_args[setup_args.index("--channel") + 1] = selected
        setup_args[setup_args.index("--update-base-url") + 1] = base
        if current.get("launch_prefix"):
            setup_args += ["--launch-prefix", *current["launch_prefix"]]
        return control(setup_args)
    if args.install_root is None:
        args.install_root = default_install_root(getattr(args, "channel", None) or invocation_channel())
    if args.command == "setup":
        install(args)
        return 0
    if args.command == "windows-launcher":
        print(WINDOWS_SESSION_LAUNCHER)
        return 0
    if args.command == "update-registered":
        print(json.dumps(update_registered(args.source_base_url)))
        return 0
    settings = load(args.install_root.expanduser().resolve() / "settings.json")
    bind_channel(settings, os.environ)
    if args.command in {"doctor", "status"}:
        healthy = doctor(settings, args)
        return 0 if healthy or not args.strict else 1
    elif args.command == "workspace":
        return workspace_control(settings, args)
    elif args.command == "update":
        if args.check_only and args.reconnect:
            parser.error("--check-only cannot be combined with --reconnect")
        if not args.check_only:
            wait_for_payloads(settings)
        result = updates.check(settings, force=True, check_only=args.check_only)
        if not args.check_only:
            result["reconnect"] = reconnect_after_update(settings, result, args.reconnect)
        print(json.dumps(result))
        return 1 if result["status"] == "failed" or result.get("reconnect", {}).get("status") == "manual_reconnect_required" else 0
    elif args.command == "register":
        register_installation(settings)
        print(json.dumps(dict(status="registered", install_root=settings["install_root"])))
    elif args.command == "reconnect":
        result = reconnect(settings, apply=args.apply, status=args.status, cancel=args.cancel,
                           pid=args.pid, wait_seconds=args.wait_seconds)
        print(json.dumps(result))
        return 1 if result["status"] == "manual_reconnect_required" else 0
    elif args.command == "discovery-router":
        print(discovery_router(settings, args.launch_prefix), end="")
    elif args.command == "uninstall":
        uninstall(settings, args.apply)
    elif args.command == "rollback":
        root = Path(settings["install_root"])
        with lock(root / ".install.lock"):
            previous = load(root / "previous-settings.json")
            if previous["state_home"] != settings["state_home"]:
                fail("rollback cannot change state-home identity")
            for name, expected in previous["files"].items():
                if not (root / name).is_file() or digest(root / name) != expected:
                    fail("previous release integrity check failed; rollback was not applied")
            save(root / "snapshot-roots.json", dict(format=FORMAT, roots=previous["roots"]))
            previous["update_policy"] = "manual"
            save(root / "settings.json", previous)
            save(root / "previous-settings.json", settings)
        print(json.dumps({"release": previous["release"], "restart_required": "only the affected connection, when ready"}))
    return 0


def main():
    mode, arguments = sys.argv[1], sys.argv[2:]
    if mode == "control":
        return control(arguments)
    settings = settings_for_runtime()
    bind_channel(settings, os.environ)
    if mode == "adapter":
        wait_for_payloads(settings)
        if (os.environ.get("AI_FENCE_CODEX_DESKTOP", "1").lower() not in OFF and
                subcommand(arguments)[0] == "app-server" and not any(arg in {"--help", "-h", "--version", "-V"} for arg in arguments)):
            result = updates.check(settings)
            try:
                register_installation(load(Path(settings["install_root"]) / "settings.json"))
                outcome = reconnect_after_update(settings, result)
                if outcome["status"] == "scheduled":
                    print("ai-fence codex: older desktop connections will reconnect when idle; reopen their saved chats", file=sys.stderr)
                elif outcome["status"] == "manual_reconnect_required":
                    print("ai-fence codex: reconnect older adapters manually once to enable automatic reconnects", file=sys.stderr)
            except (OSError, ValueError, RuntimeError):
                print("ai-fence codex: automatic reconnect deferred; use doctor or reconnect --status", file=sys.stderr)
            if result["status"] == "installed":
                selected = load(Path(settings["install_root"]) / "settings.json")
                os.environ["_AI_FENCE_DESKTOP_SETTINGS"] = json.dumps(selected)
                script = cli_payload(selected).with_name("runtime.py")
                os.execv(sys.executable, [sys.executable, str(script), mode, *arguments])
        with lock(Path(settings["install_root"]) / ".lifecycle.lock", shared=True):
            if not (Path(settings["install_root"]) / "settings.json").exists():
                fail("integration was uninstalled before this launch; use native Codex or rerun setup")
            return adapter(settings, arguments)
    if mode == "runtime":
        runtime(settings, arguments)
    if mode == "snapshot":
        cli = cli_payload(settings)
        os.execv(str(cli), [str(cli), "snapshot", *arguments])
    fail("unknown runtime mode")


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        # Never include captured child stdout/stderr, RPC payloads or environments.
        print("ai-fence codex: " + (str(error) if isinstance(error, RuntimeError) else type(error).__name__), file=sys.stderr)
        raise SystemExit(2)
