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
        if old:
            for name, expected in old["files"].items():
                owned = root / name
                if owned.is_symlink() or not owned.is_file() or digest(owned) != expected:
                    fail("owned installation file changed; preserve and inspect it before upgrading")
        elif (root / "dispatch.py").exists():
            fail("unowned dispatch file exists")
        for name in ["codex", "codex-runtime", "ai-fence-snapshot"]:
            candidate = root / "bin" / name
            if candidate.is_symlink() or (candidate.exists() and not old):
                fail("unowned adapter command exists; choose an empty install root")
        home = args.state_home.expanduser().resolve()
        home.mkdir(parents=True, exist_ok=True, mode=0o700)
        if old and (old["state_home"] != str(home) or old["profile"] != args.profile):
            fail("upgrade cannot change profile/state-home identity; use a separate installation")
        profile_name(args.profile)
        if args.snapshots != "off" and args.approve_root and not args.snapshot_directory:
            fail("approved roots require --snapshot-directory on the same Btrfs filesystem")
        files = {}
        runtime = Path(__file__).read_bytes()
        release_id = hashlib.sha256(runtime + digest(cli).encode()).hexdigest()[:24]
        release = root / "releases" / release_id
        release.mkdir(parents=True, exist_ok=True, mode=0o700)
        # Content-addressed releases retain binaries used by running processes.
        for name, data, mode in [("runtime.py", runtime, 0o700), ("codex-session.ps1", WINDOWS_SESSION_LAUNCHER.encode(), 0o600)]:
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
            body = ("#!/bin/sh\n" + MARKER + "\nexec " + shell(sys.executable) + " "
                    + shell(str(root / "dispatch.py")) + " " + mode + ' "$@"\n')
            if wrapper.exists() and MARKER not in wrapper.read_text():
                fail("unowned adapter command exists; choose an empty install root")
            atomic(wrapper, body, 0o700)
            files[str(wrapper.relative_to(root))] = digest(wrapper)
        # Stable dispatch selects one immutable release per invocation.
        dispatch = 'import json,os,sys\nfrom pathlib import Path\nr=Path(__file__).resolve().parent\ns=json.loads((r/"settings.json").read_text())\nos.execv(sys.executable,[sys.executable,str(r/"releases"/s["release"]/"runtime.py"),*sys.argv[1:]])\n'
        if (root / "dispatch.py").exists() and old is None:
            fail("unowned dispatch file exists")
        atomic(root / "dispatch.py", dispatch, 0o700)
        files["dispatch.py"] = digest(root / "dispatch.py")
        settings = dict(format=FORMAT, install_root=str(root), release=release_id,
                        native_codex=str(native), native_version=version, native_sha256=digest(native), no_daemon_supported=no_daemon_supported,
                        profile=profile_name(args.profile), state_home=str(home), state_identity=identity(home), readiness_seconds=args.readiness_seconds,
                        seed_home=str(args.seed_home.expanduser().resolve()) if args.seed_home else str(home),
                        snapshot_policy=args.snapshots, roots=[str(p.expanduser().absolute()) for p in args.approve_root],
                        snapshot_directory=str(args.snapshot_directory.expanduser().absolute()) if args.snapshot_directory else None,
                        files=files)
        if settings["snapshot_policy"] != "off" and settings["roots"] and not settings["snapshot_directory"]:
            fail("approved roots require --snapshot-directory on the same Btrfs filesystem")
        save(root / "snapshot-roots.json", dict(format=FORMAT, roots=settings["roots"]))
        if old and old != settings:
            save(root / "previous-settings.json", old)
        if args.snapshots != "off":
            install_guidance(home)
        # This is the only release switch; existing children retain old settings.
        save(settings_path, settings)
        print(json.dumps({"installed": True, "adapter": str(root / "bin/codex"),
                          "profile": settings["profile"], "state_home": str(home),
                          "native_version": version, "release": release_id,
                          "discovery": "select this adapter for the WSL connection; desktop discovery varies by version"}))


def shell(value):
    return "'" + value.replace("'", "'\"'\"'") + "'"


def cli_payload(settings):
    return Path(settings["install_root"]) / "releases" / settings.get("release", "fixture") / "ai-fence-cli"


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
    env = os.environ.copy()
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


def attach(settings, thread_id, cwd, lease, checkpoint=None, mode="checkpoint"):
    path = record_path(settings, thread_id)
    with lock(path.with_suffix(".lock")):
        project = identity(cwd)
        if path.exists():
            record = load(path)
            if not Path(record["workspace"]["physical"]).exists():
                fail("saved workspace is missing; recover to a new workspace from a chosen checkpoint or explicitly adopt a replacement")
            if record["workspace"] != project:
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


class Lifecycle:
    """Observe the actual protocol; never persist inputs, outputs or transcripts."""
    def __init__(self, settings, lease):
        self.settings, self.lease = settings, lease
        self.pending = {}
        self.gate = threading.Lock()
        self.threads = set()

    def request(self, value):
        method = value.get("method")
        parameters = value.get("params") or {}
        if method in {"thread/resume", "turn/start"}:
            thread_id = parameters.get("threadId")
            path = record_path(self.settings, thread_id) if thread_id else None
            if path and path.exists():
                with lock(path.with_suffix(".lock")):
                    preflight_resume(self.settings, parameters)
                    record = load(path)
                    record["leases"] = [v for v in record["leases"] if alive(v) and v != self.lease]
                    record["leases"].append(self.lease)
                    save(path, record)
            else:
                preflight_resume(self.settings, parameters)
        if method in {"thread/start", "thread/resume", "thread/fork"} and "id" in value:
            with self.gate:
                self.pending[str(value["id"])] = method

    def response(self, value):
        with self.gate:
            method = self.pending.pop(str(value.get("id")), None)
        thread = (value.get("result") or {}).get("thread") if method else None
        if value.get("method") == "thread/started":
            thread = value.get("params", {}).get("thread")
        if isinstance(thread, dict) and thread.get("id") and thread.get("cwd"):
            thread_id, cwd = thread["id"], thread["cwd"]
            receipt = checkpoint(self.settings, cwd, thread_id) if thread_id not in self.threads else None
            attach(self.settings, thread_id, cwd, self.lease, receipt)
            self.threads.add(thread_id)


def adapter(settings, arguments):
    if os.environ.get("_AI_FENCE_DESKTOP_OWNED") == "1":
        fail("recursive desktop adapter dispatch; native executable/runtime must be absolute")
    wait_for_payloads(settings)
    env = os.environ.copy()
    env.pop("CODEX_INSTALL_DIR", None)
    env.pop("CODEX_CLI_PATH", None)
    command, position = subcommand(arguments)
    tail = arguments[position + 1:] if position is not None else []
    bypass = env.get("AI_FENCE_CODEX_DESKTOP", "1").lower() in OFF
    discovery = any(x in {"--version", "-V", "--help", "-h"} for x in arguments)
    native_commands = {"login", "logout", "completion", "migrate-rollouts", "features", "debug", "help"}
    if bypass or discovery or command in native_commands or (tail and tail[0] in {"generate-ts", "generate-json-schema"}):
        # Keep CLI entry points in the selected history store as well.
        env["CODEX_HOME"] = settings["state_home"]
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
    cli = cli_payload(settings)
    seed_auth(settings)
    argv = [str(cli), "run", "--no-codex-config", "--standalone", "--localsubscription", "--agent", "codex",
            "--profile", settings["profile"], "--codex-home", settings["state_home"],
            "--codex-auth-source", "managed", "--", str(cli.parent / "codex-runtime"), *arguments]
    lease = dict(pid=os.getpid(), start=process_identity(os.getpid()))
    live = root / "launches" / (str(os.getpid()) + ".json")
    save(live, dict(format=FORMAT, **lease, child_pid=None, profile=settings["profile"],
                    state_home=settings["state_home"], logical_cwd=os.environ.get("PWD", os.getcwd()),
                    physical_cwd=str(Path.cwd().resolve()), mode="checkpoint", endpoint=None, release=settings["release"]))
    env["_AI_FENCE_DESKTOP_LAUNCH"] = str(live)
    interactive = command != "app-server"
    if interactive and command == "resume" and tail and re.fullmatch(r"[a-fA-F0-9-]{36}", tail[0]):
        parameters = dict(threadId=tail[0])
        preflight_resume(settings, parameters)
        if parameters.get("cwd"):
            argv = argv[:argv.index("--") + 2] + ["-C", parameters["cwd"], *arguments]
            attach(settings, tail[0], parameters["cwd"], lease)
    child = subprocess.Popen(argv, env=env, stdin=None if interactive else subprocess.PIPE,
                             stdout=None if interactive else subprocess.PIPE,
                             start_new_session=True, bufsize=0)
    with lock(live.with_suffix(".lock")):
        item = load(live)
        item["child_pid"] = child.pid
        save(live, item)
    lifecycle = Lifecycle(settings, lease)
    output_lock = threading.Lock()
    def send(value):
        with output_lock:
            sys.stdout.buffer.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
            sys.stdout.buffer.flush()
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
                    try:
                        lifecycle.request(value)
                    except (RuntimeError, OSError, ValueError):
                        send({"jsonrpc": "2.0", "id": value.get("id"), "error": {
                            "code": -32602, "message": "AI Fence saved workspace unavailable or changed; explicit recovery/adoption required"}})
                        continue
                    child.stdin.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
                    child.stdin.flush()
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
            live.unlink(missing_ok=True)
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    buffer = b""
    try:
        while True:
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
                try:
                    lifecycle.response(value)
                except (RuntimeError, OSError, ValueError, subprocess.SubprocessError):
                    print("ai-fence codex: thread workspace registration/checkpoint failed", file=sys.stderr)
                    stop(signal.SIGTERM, None)
                    break
                send(value)
        status = child.wait(timeout=5)
    finally:
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
    report = dict(format=FORMAT, profile=settings["profile"], state_home=str(home),
                  native_path=settings["native_codex"], native_version=settings["native_version"],
                  native_unchanged=Path(settings["native_codex"]).is_file() and digest(settings["native_codex"]) == settings["native_sha256"],
                  owned_files=files, launches=launches, logical_cwd=os.environ.get("PWD", os.getcwd()),
                  physical_cwd=str(Path.cwd().resolve()), mode="checkpoint", snapshot_policy=settings["snapshot_policy"],
                  sessions_present=(home / "sessions").is_dir(),
                  resume="resume --all removes cwd filtering only within this CODEX_HOME; provider/account filters are separate",
                  sqlite_home=str(Path(sqlite_home).expanduser().resolve()),
                  saved_provider=native_config.get("model_provider", "openai"),
                  effective_fenced_provider="ai_fence (process override)",
                  active_native_configuration="per-launch overrides; owning endpoint is recorded separately")
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
    report["healthy"] = all(files.values()) and report["native_unchanged"] and all(
        not v["alive"] or (v.get("listener_owned", False) and v.get("endpoint_tcp_alive", False)) for v in launches)
    if args.endpoint:
        report["healthy"] = report["healthy"] and report["endpoint_tcp_alive"]
    if args.thread_id:
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


def git_independent(root):
    root = Path(root).resolve(strict=True)
    if not (root / ".git").exists():
        return True
    if (root / ".git").is_symlink() or not (root / ".git").is_dir():
        return False
    for relative in ["--git-dir", "--git-common-dir", "--show-toplevel"]:
        result = subprocess.run(["git", "-C", str(root), "rev-parse", "--path-format=absolute", relative],
                                capture_output=True, timeout=5)
        if result.returncode or not Path(os.fsdecode(result.stdout).strip()).resolve().is_relative_to(root):
            return False
    if any((root / ".git").rglob("alternates")):
        return False
    # Nested Git metadata (submodules/worktrees) needs deliberate relocation.
    for parent, directories, files in os.walk(root, followlinks=False):
        if Path(parent) == root:
            directories[:] = [d for d in directories if d != ".git"]
        elif ".git" in directories or ".git" in files:
            return False
    return True


def workspace_control(settings, args):
    path = record_path(settings, args.thread_id) if getattr(args, "thread_id", None) else None
    if args.workspace_action == "cleanup":
        records = sorted((Path(settings["state_home"]) / "ai-fence-workspaces").glob("*.json"))
        items = []
        for p in records[:args.limit]:
            r = load(p)
            items.append(dict(thread_id=r["thread_id"], workspace=r["workspace"]["physical"],
                              protected="chat-linked", active=any(alive(v) for v in r["leases"]), pinned=r["pinned"]))
        print(json.dumps(dict(preview=True, workspaces=items, truncated=len(records) > args.limit,
                              note="No linked, active, dirty, pinned or unowned workspace is eligible. No workspace deletion is performed."), indent=2))
        return
    with lock(path.with_suffix(".lock")):
        existing = load(path) if path.exists() else None
        if existing and any(alive(v) for v in existing["leases"]):
            fail("workspace has an active lease; adoption/recovery cannot change it")
        thread = native_thread(settings, args.thread_id)
        if not thread or not thread.get("cwd"):
            fail("thread absent from selected CODEX_HOME; inspect the original state store before adoption")
        source = args.project.expanduser().resolve(strict=args.workspace_action != "recover")
        if args.workspace_action != "recover" and identity(thread["cwd"]) != identity(source):
            fail("native thread belongs to another project; adoption cannot rewrite historical cwd or account/provider metadata")
        project = existing["project"] if existing else identity(source)
        destination = source
        receipt = None
        if args.workspace_action == "create":
            if not any(Path(p).resolve() == source for p in settings["roots"]):
                fail("writable isolation requires explicit --approve-root in setup")
            args.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            destination = args.directory.resolve() / args.thread_id
            if existing and existing["mode"] == "isolated":
                if not Path(existing["workspace"]["physical"]).exists() or identity(existing["workspace"]["physical"]) != existing["workspace"]:
                    fail("saved workspace is missing or replaced; explicit recovery is required")
                print(json.dumps(dict(reused=True, workspace=existing["workspace"]["physical"])))
                return
            if destination.exists():
                fail("unregistered destination exists; adopt explicitly instead of replacing files")
            cli = cli_payload(settings)
            inventory = subprocess.run([str(cli), "snapshot", "inspect", "--root", str(source),
                                        "--directory", str(args.directory.resolve()), "--recursive"],
                                       capture_output=True, timeout=15, check=True)
            scope = json.loads(inventory.stdout)
            if scope["nested"]:
                fail("nested subvolumes require coordinated reconstruction; writable cloning is unavailable for this project")
            if not git_independent(source):
                fail("external/shared Git metadata is not isolated; use native Git worktrees or an explicit shared workspace")
        elif args.workspace_action == "recover":
            manifest = json.loads(args.checkpoint.read_text())
            if not manifest.get("complete") or not manifest.get("read_only") or len(manifest.get("members", [])) != 1:
                fail("recovery requires a complete single-member read-only checkpoint; reconstruct nested sets explicitly")
            if Path(manifest["root"]).resolve() != source:
                fail("checkpoint project identity does not match the selected project")
            receipt = dict(id=manifest["id"], path=manifest["path"])
            snapshot = Path(manifest["path"]) / manifest["members"][0]["snapshot_name"]
            if snapshot.parent.resolve() != args.checkpoint.parent.resolve() or not git_independent(snapshot):
                fail("checkpoint location or Git independence cannot be verified")
            if identity(snapshot)["subvolume_uuid"] != manifest["members"][0]["snapshot_uuid"]:
                fail("checkpoint subvolume identity changed")
            readonly = subprocess.run(["btrfs", "property", "get", "-ts", str(snapshot), "ro"], capture_output=True, timeout=5, check=True)
            if readonly.stdout.strip() != b"ro=true":
                fail("chosen checkpoint is no longer read-only")
            source = snapshot
            args.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            destination = args.directory.resolve() / (args.thread_id + "-recovered-" + manifest["id"])
            if destination.exists():
                fail("recovery destination already exists; no replacement performed")
        elif args.workspace_action == "adopt" and args.workspace:
            destination = args.workspace.expanduser().resolve(strict=True)
        plan = dict(preview=not args.apply, thread_id=args.thread_id, project=project,
                    source=str(source), workspace=str(destination),
                    preserve_current=existing["workspace"] if existing else identity(args.project),
                    mode="isolated" if args.workspace_action in {"create", "recover"} else "shared")
        print(json.dumps(plan, indent=2))
        if not args.apply:
            return
        if args.workspace_action in {"create", "recover"}:
            subprocess.run(["btrfs", "subvolume", "snapshot", str(source), str(destination)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30, check=True)
            if not git_independent(destination):
                fail("created workspace failed Git independence verification; preserved for inspection, not registered")
        if existing:
            save(path.with_name(path.stem + ".previous.json"), existing)
        record = dict(format=FORMAT, thread_id=args.thread_id, profile=settings["profile"],
                      state_home=settings["state_home"], project=project, logical_root=str(args.project),
                      workspace=identity(destination), mode=plan["mode"],
                      checkpoints=(existing["checkpoints"] if existing else []) + ([receipt] if receipt else []),
                      leases=[], pinned=existing["pinned"] if existing else False)
        save(path, record)


def control(arguments):
    parser = argparse.ArgumentParser(prog="ai-fence-cli codex-desktop")
    parser.add_argument("--install-root", type=Path, default=Path.home() / ".local/share/ai-fence-codex-desktop")
    commands = parser.add_subparsers(dest="command", required=True)
    setup = commands.add_parser("setup", help="install an opt-in desktop adapter without changing native Codex")
    setup.add_argument("--native-codex", required=True, type=Path)
    setup.add_argument("--state-home", required=True, type=Path, help="explicit stable Codex state store; never a run directory")
    setup.add_argument("--readiness-seconds", type=int, choices=range(0, 121), default=45, help="bounded passive wait for configured native/state storage")
    setup.add_argument("--seed-home", type=Path, help="optional same-account native auth seed; existing managed auth wins")
    setup.add_argument("--profile", default=os.environ.get("AI_FENCE_CODEX_PROFILE", os.environ.get("AI_FENCE_PROFILE", "default")))
    setup.add_argument("--snapshots", choices=["auto", "off", "required"], default="auto")
    setup.add_argument("--approve-root", type=Path, action="append", default=[])
    setup.add_argument("--snapshot-directory", type=Path)
    status = commands.add_parser("doctor", aliases=["status"])
    status.add_argument("--strict", action="store_true", help="exit nonzero on unhealthy file/route metadata")
    status.add_argument("--endpoint")
    status.add_argument("--thread-id")
    remove = commands.add_parser("uninstall")
    remove.add_argument("--apply", action="store_true")
    workspace = commands.add_parser("workspace", help="preview adoption, persistent isolation or recovery")
    actions = workspace.add_subparsers(dest="workspace_action", required=True)
    for name in ["adopt", "create", "recover"]:
        action = actions.add_parser(name)
        action.add_argument("--thread-id", required=True)
        action.add_argument("--project", type=Path, required=True)
        action.add_argument("--apply", action="store_true")
        if name == "adopt":
            action.add_argument("--workspace", type=Path)
        else:
            action.add_argument("--directory", type=Path, required=True)
        if name == "recover":
            action.add_argument("--checkpoint", type=Path, required=True, help="chosen checkpoint manifest.json")
    cleanup = actions.add_parser("cleanup")
    cleanup.add_argument("--limit", type=int, choices=range(1, 101), default=32)
    commands.add_parser("rollback", help="select previous installed release; does not restore history or workspaces")
    commands.add_parser("windows-launcher", help="print the packaged session-only Windows launcher")
    args = parser.parse_args(arguments)
    if args.command == "setup":
        install(args)
        return 0
    if args.command == "windows-launcher":
        print(WINDOWS_SESSION_LAUNCHER)
        return 0
    settings = load(args.install_root.expanduser().resolve() / "settings.json")
    if args.command in {"doctor", "status"}:
        healthy = doctor(settings, args)
        return 0 if healthy or not args.strict else 1
    elif args.command == "workspace":
        workspace_control(settings, args)
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
            save(root / "settings.json", previous)
            save(root / "previous-settings.json", settings)
        print(json.dumps({"release": previous["release"], "restart_required": "only the affected connection, when ready"}))
    return 0


def main():
    mode, arguments = sys.argv[1], sys.argv[2:]
    if mode == "control":
        return control(arguments)
    settings = settings_for_runtime()
    if mode == "adapter":
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
