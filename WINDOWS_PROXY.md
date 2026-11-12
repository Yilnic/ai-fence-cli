# Native Windows proxy installation

Use `scripts/install-windows-proxy.ps1` for a persistent native Windows Codex
subscription proxy. This is separate from the Linux/WSL stdio adapter. The
installer uses a per-user Task Scheduler task with three failure restarts, one
minute apart, no execution time limit, and a sign-in trigger. The supervisor
also restarts a failed proxy child. Startup and upgrades must use the installed
`start-proxy.ps1`; spawning `proxy_service.py` directly bypasses task recovery.

Download the official Windows executable and its matching
`/downloads/ai-fence-cli-manifest.json?target=windows-x86_64` manifest from the
selected release server. Keep the manifest and executable together. The installer
checks channel, target, filename, size and SHA-256 before creating a task. Supply
a stable Windows Python 3.11+ installation and a file containing your AI Fence
API key. Avoid a Python executable inside an application's temporary cache.

New CLI builds bundle the installer, including on native Windows. Export it
without requiring Python or WSL:

```powershell
ai-fence-cli codex-desktop windows-proxy-installer --directory .\proxy-installer
```

Run `proxy-installer\install-windows-proxy.ps1` with the arguments below. A source
checkout also provides the same script at `scripts/install-windows-proxy.ps1`.
In Windows PowerShell:

```powershell
.\proxy-installer\install-windows-proxy.ps1 `
  -Root "$env:LOCALAPPDATA\ai-fence-windows-proxy" `
  -Python 'C:\Program Files\Python312\python.exe' `
  -CliPath '.\downloads\ai-fence-cli-windows-x86_64.exe' `
  -ManifestPath '.\downloads\manifest.json' `
  -ApiKeyPath '.\private\api-key' `
  -FenceUrl 'https://ai.yaaf.de' -Channel prod -Port 60441
```

Use `https://ai-dev.yaaf.de` and `-Channel dev` with the DEV release. The root
must be a new or empty local directory. Its files receive a private directory
ACL. Task names derive from the installation root, so separate installations
do not replace each other's tasks. The task runs with the current user's normal
privileges while that user is signed in; it does not require a stored password.
`-NoStart` provisions the installation without requesting startup.

The installer does not edit Codex settings or authentication. After verifying
`http://127.0.0.1:60441/api/v1/health`, set the native Windows Codex
`openai_base_url` to `http://127.0.0.1:60441/v1` when ready to use it.

## Repair and future upgrades

Run the same repository installer against an existing root:

```powershell
.\proxy-installer\install-windows-proxy.ps1 -Root 'C:\path\to\existing\proxy' -Repair
```

For a legacy installation without a saved task name, supply the exact existing
task once with `-TaskName 'existing task name'`. Repair validates its executable,
arguments, working directory and owning user before changing anything. It
updates the task's recovery settings and lifecycle scripts, retaining release,
API key, endpoint, port and live processes. A healthy managed supervisor stays
running. The new supervisor code takes effect at the next normal service start.
An unmanaged running supervisor requires an explicit managed cutover.

The legacy `start-dev-proxy.ps1` entry point is upgraded too when present, so
existing updaters retain task-managed startup. Future binary updates must stop
cooperatively through `stop-proxy`, wait for the old supervisor to finish, select
the verified release, and invoke `start-proxy.ps1`. The launcher waits for a
stopping task to settle before starting it, preventing `IgnoreNew` from losing
the request. Never replace startup with `Start-Process` or `subprocess.Popen`.

`service.stderr.log` records supervisor starts, child exit codes, cooperative
stops and fatal exceptions; proxy output remains in `proxy.stdout.log` and
`proxy.stderr.log`. Hard termination of Windows itself may leave no final entry.

Run isolated Windows lifecycle tests with
`scripts/test-windows-proxy.ps1 -Python C:\path\to\python.exe`. Task Scheduler
calls are mocked; the tests do not stop real proxies or change real tasks.
