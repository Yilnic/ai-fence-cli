# Codex desktop and native WSL setup

The named workspace interface below starts with v0.1.8; v0.1.7 retains
its original workspace commands and checkpoint defaults.

This opt-in target is bundled in `ai-fence-cli`; it requires Linux/WSL and
Python 3.11+. Install the CLI normally, then run the target below. It preserves
the genuine native executable and explicitly selects a durable Codex state
store. Setup does not activate a running desktop connection or replace its
proxy. Native Windows connections remain available.

```bash
ai-fence-cli codex-desktop --install-root "$HOME/.local/share/ai-fence-codex-desktop" setup \
  --native-codex /absolute/path/to/genuine/codex \
  --state-home /absolute/path/to/chosen/codex/state \
  --auth-home "${CODEX_HOME:-$HOME/.codex}" \
  --profile personal
```

Choose the existing store containing the conversations you want to resume.
`CODEX_HOME` contains native auth, configuration, sessions and indexes. Existing
AI Fence stores usually live under
`$AI_FENCE_HOME/profiles/PROFILE/codex/state`; the native default is `~/.codex`.
Setup requires an explicit store so it cannot silently start a fresh history.
Profile-auth installations support CLI and desktop entry points with that store.
Shared native authentication is currently for the desktop stdio app-server;
ordinary native CLI launches keep their native home. The profile default is
`AI_FENCE_CODEX_PROFILE`, then `AI_FENCE_PROFILE`, then
`default`; selecting a profile never merges stores or accounts.

The native path is resolved to its physical executable. AI Fence never renames
or overwrites it. A native upgrade can select the new genuine executable by
rerunning setup with the same state home and profile. Existing app-servers keep
their executable, selected release, state home and process-owned endpoint.
Setup checks owned file hashes and refuses edited/unowned commands. Releases
are content-addressed and retained for rollback. Repeating identical setup is
idempotent. The payload includes the CLI, adapter, runtime helper, snapshot
command dispatcher and a Windows session launcher; Python is a prerequisite.

## DEV channel

Install and configure the DEV CLI first:

```bash
curl -fsSL https://ai-dev.yaaf.de/install.sh | bash
ai-fence-cli-dev setup --url https://ai-dev.yaaf.de --mode backend
ai-fence-cli-dev codex-desktop setup \
  --native-codex /absolute/path/to/genuine/codex \
  --state-home /absolute/path/to/chosen/codex/state \
  --auth-home /absolute/path/to/native/auth/home
```

DEV is an installation channel, not a profile. The default adapter directory is
`~/.local/share/ai-fence-codex-desktop-dev`; PROD retains
`~/.local/share/ai-fence-codex-desktop`. Setup reads the update URL from the
channel's saved CLI configuration (or explicit `--update-base-url`) and persists
the channel. Future desktop launches retain that channel even when the app's
environment selects PROD. DEV follows verified build hashes, including changed
builds with the same package version; PROD retains version-only immutable release
updates. `doctor` reports the selected channel and update URL.

To switch an existing single adapter without re-entering its history/account or
admission configuration, select its exact installation root:

```bash
ai-fence-cli-dev codex-desktop --install-root /existing/adapter/root switch-channel
```

This reuses the saved state home, auth home, profile, checkpoint approvals and
admission launcher. The existing `rollback` command can restore its previous
code/channel selection; reconnect when ready. Manual `setup` also accepts the
existing root; retain its exact `--state-home`, `--auth-home` and `--profile` values.
Use the existing admission launcher as `--launch-prefix` if configured; upgrades
retain it when omitted. Explicit setup may switch the channel, but automatic
updates cannot change the channel, update source, history, authentication owner,
or admission policy. No profile named `dev` is created or required.

Setup does not replace a running connection. Reconnect the selected app/WSL
connection when ready; only one selected adapter needs to be active. Keep the
existing adapter path/router to preserve discovery, or select the DEV adapter's
`bin` directory for a separate installation. Use `ai-fence-cli-dev codex-desktop
--install-root /existing/adapter/root doctor --strict` to inspect the result.
This is the app's Codex/WSL stdio proxy, not a general ChatGPT network proxy.

## Native authentication and state

Authentication stays native, with one explicit credential owner. A fresh setup
uses the native `CODEX_HOME` (normally `~/.codex`) for credentials while
`--state-home` retains the existing conversations. Specify `--auth-home` once
to make the selection explicit; upgrades retain the installed selection. To keep
an independent account in the history profile, select that same path for both
options. An upgrade of an older installation retains profile auth until you
explicitly select the native owner.

In shared-native mode, a small auth-only native app-server owns browser/device
login, logout and OAuth refresh in `--auth-home`. The history app-server uses
Codex's supported external-token login/refresh API with ephemeral storage.
Only access tokens are supplied in memory; refresh tokens are never copied to
the history home. A stale/revoked profile file is preserved and ignored by this
process. Desktop re-login and `INSTALL_ROOT/bin/codex login` target the same
credential owner. Account mismatches fail before binding; a later native login
to another account cannot silently change this history profile's account.

This mode requires native file credential storage and the external-token API;
setup checks the installed schema. Keyring/auto stores, API keys and native TUI
through this adapter use profile auth instead. Generic `run --codex-auth-source
auto` still means bootstrap-if-missing, not credential synchronization. It does
not repair another profile's login. No SQLite stores or histories are merged.
Config is preserved: provider, Responses transport and owning loopback endpoint
are process overrides. Desktop launches do not write ephemeral routes into the
shared `config.toml`. Concurrent profiles can use separate install roots/stores;
concurrent launches of one installation receive independent proxies.

The native subscription WebSocket route currently rejects `response.steer`
locally because its upstream steering capability is not established. Updating a
proxy alone does not enable that route. Queued next-turn input and app-server
`turn/steer` acceptance are separate from upstream mid-response steering. See
[the steering API](https://developers.openai.com/api/docs/guides/steering) for the
OpenAI API contract; it does not establish support at every subscription endpoint.

## Automatic proxy updates

New official installations default to `--updates auto`. On a future stdio
app-server launch, the adapter checks the trusted HTTPS release manifest at most
once per day. Network/download failures back off five minutes and retain the
working release. Checks and downloads have bounded timeouts; concurrent launches
use one update lock and other launches proceed with the current release.
There is no background timer or recurring monitoring.
Launch updates currently support Linux x86_64; other architectures report
`unsupported_target` without downloading or executing an incompatible binary.

An update verifies target, version, size, SHA-256 and executable version, then
asks the new official CLI to prepare its immutable release. It verifies unchanged
history/auth/profile/checkpoint policy and router files before one atomic settings
switch. Concurrent settings changes cancel activation, including changes that
retain the same release. Old payloads remain available to active processes and rollback. Updating
never restarts a chat, desktop, proxy, WSL, or memory admission service. A running
connection adopts new code only after its normal reconnect; a newer download alone
does not change an executable already mapped into a process.

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root update --check-only
ai-fence-cli codex-desktop --install-root /absolute/install/root update
ai-fence-cli codex-desktop --install-root /absolute/install/root doctor --check-auth
```

`doctor` distinguishes installed CLI version/hash from each running launch's
version/hash and reports pending older processes, auth home and last update status.
`--check-auth` additionally makes one read-only authenticated provider models
request; TCP connectivity and history reads alone do not prove valid model auth.
No credentials or provider response bodies appear in diagnostics.

Use setup `--updates manual` or `--updates off` to disable launch checks; manual
`update` remains explicit. A rollback sets updates to manual so the next launch
cannot immediately undo it. Re-enable auto through setup when ready. Native Codex
upgrades and changes to discovery/admission wrappers require explicit setup;
auto-update will preserve modified or changed routers instead of overwriting them.
Checksums identify release bytes; they are not a publisher signature.

If the host already uses a memory/admission broker, supply its existing command
prefix once during setup, LAST: `--launch-prefix /absolute/existing/launcher --`.
The installer retains that prefix on upgrades and applies it before loading the
adapter or checking for updates. Automatic activation refuses an admission-policy
change. No broker, cap or nesting rule is installed or changed by this option.

These mechanisms apply to the official installation. Existing custom pinned
payloads do not gain automatic updates merely by updating the normal CLI. Migrate
them through a reviewed setup when their running sessions are ready; preserve any
outer memory/admission launcher. Publishing a CLI release does not migrate or
activate a replacement for those custom installations.

## Connect the Windows desktop

Use the installed `INSTALL_ROOT/bin/codex` for the WSL connection. Discovery is
version dependent: setup deliberately does not edit Windows configuration,
change default CLI aliases, or download a native CLI over the adapter.

For desktop builds that discover the default `codex` command, generate an
optional router for review. It parses the native command after global options,
routes all `app-server` invocations to this installation, and preserves ordinary
native CLI arguments and environment. It does not depend on one exact desktop
feature flag, flag order or installed app version:

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root discovery-router > codex-router.py
# With an existing memory/admission launcher, preserve it on BOTH routes:
ai-fence-cli codex-desktop --install-root /absolute/install/root discovery-router \
  --launch-prefix /absolute/existing/admission-launcher -- > codex-router.py
```

Generation does not replace your current command or activate a connection. Install
it only through the existing host's reviewed wrapper/update mechanism. Keep the
native Windows connection available. Direct/network app-server transports remain
unsupported by the desktop adapter and fail explicitly rather than selecting a
fresh native history store.

Export the packaged launcher for the Windows maintenance chat to inspect:

```bash
ai-fence-cli codex-desktop windows-launcher > codex-session.ps1
```

On Windows, the following is a **check only**. Supply the Linux adapter's bin
directory; no username, distro UUID, port or Appx version is embedded:

```powershell
.\codex-session.ps1 -Mode Fenced -LinuxAdapterDirectory '/absolute/install/root/bin' -CheckOnly
.\codex-session.ps1 -Mode Native -CheckOnly
```

The launcher discovers the current `OpenAI.Codex` Store package, or accepts
`-Executable` for another installation. Actual launch requires quitting the
existing app when its sessions are ready. It sets process environment only and
restores the calling shell afterwards. It forwards `CODEX_INSTALL_DIR/u` for
WSL discovery in fenced mode and clears CLI/install overrides in native mode.
The adapter removes `CODEX_INSTALL_DIR` before launching children so a remote
CLI installer cannot target its shim. Invoke native updates explicitly outside
this discovery environment, then rerun AI Fence setup with the new native path.

**Windows validation still required:** confirm that the installed desktop
supports native WSL remote connections, discovers the selected directory, and
initializes this adapter on the selected Linux distribution while retaining its
Windows connection. Some older desktop builds require the experimental
`features.wsl_remote_connections`; this setup does not assume that setting is
supported. No desktop restart, Windows setting change or production replacement
is part of local verification. For cold attached storage, place the adapter on
an available Linux filesystem and configure `--readiness-seconds` (0–120,
default 45). It passively waits for the configured native executable and exact
state filesystem identity and never creates a replacement state home at launch.

## Named workspaces and optional checkpoints

We recommend **Btrfs writable workspaces** for independent parallel agents.
Use the same named registry for CLI and desktop; see [the workspace guide](WORKSPACES.md)
for filesystem setup, `fws` shell convenience, manual `cd`, review and removal.

```bash
ai-fence-cli ws create feature-a --from /absolute/project/subvolume
ai-fence-cli ws run feature-a -- codex-fenced --yolo
ai-fence-cli ws path feature-a
```

Open that path as a WSL project in the desktop. The adapter reserves the actual
thread workspace before forwarding work and links native start/resume/fork
responses. CLI launches from inside the directory use the same lease automatically.
A new proxy, temporary run ID or process startup directory does not choose a
new workspace. Ordinary resume reuses the current files and saved subdirectory.
Missing/replaced workspaces require explicit recovery, never silent fallback.

The registry defaults to `~/.ai-fence/workspace-registry`, independently of
provider/account/history stores. If CLI tools use `AI_FENCE_WORKSPACE_HOME`,
select that same registry during desktop setup with `--workspace-registry PATH`.
Workspace selection does not change `CODEX_HOME`. To share conversations,
select the same existing native state store in both entry points; separate
profiles/stores are not merged or linked through workspace names.

Setup initializes `fw` (Fence workspace) and `fws` (Fence workspace switch).
Use `fw create feature-a`, `fws feature-a`, or `fw run feature-a -- codex-fenced --yolo`.
It is not required: ordinary `cd` plus
existing fenced aliases works, including from workspace subdirectories.
The desktop `codex-desktop workspace` entry forwards to the same management
engine as `ai-fence-cli workspace` rather than keeping another clone implementation.

Read-only checkpoints preserve recovery points and keep cwd unchanged.
They default to **off** in this build. Explicitly add policy during setup:

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root setup \
  --native-codex /absolute/native/codex --state-home /absolute/state --profile personal \
  --snapshots auto --approve-root /absolute/project/subvolume \
  --snapshot-directory /absolute/same-btrfs/checkpoints
```

An inherited `AI_FENCE_SNAPSHOTS=auto` or `required` still overrides the configured
policy. Use `AI_FENCE_SNAPSHOTS=off` for a session-only opt-out. Exact approved
roots remain required; named workspace creation does not silently add roots to
an existing checkpoint allowlist. `required` failure closes the owning stdio
connection before forwarding its thread response, which can affect other threads
on that connection. Nested checkpoints are sequential sets; pause writers for
consistency. Conversation and filesystem references are not automatically the
same point in time.

The snapshot dispatcher exposes status, inspect, create, list, pin and prune.
Prune previews by default and caps each invocation at 32 sets (`--limit`).
It protects pinned/incomplete/recent/newest sets. There is no automatic deletion
or total-retention cap. Desktop chat-referenced read-only checkpoints are pinned.
Writable workspaces have their own guarded `workspace rm` preview and explicit
application, described in the workspace guide. Existing workspaces/releases are
retained when connections close.

Interactive native CLI pickers do not expose thread IDs to their parent.
Their selected native store is recorded to protect workspace removal without
inventing identities. The desktop can register such a thread using native metadata
when it is opened. `resume --all`, explicit native session IDs and directory
flags remain agent arguments. Known registered IDs require their saved workspace
for CLI resume; use `fws NAME` first. Provider/account/cwd filters remain separate.

Daemon and network-listener app-server transports are rejected because they
can outlive the owning proxy; desktop stdio is supported. Setup probes native
`--no-daemon` support for interactive CLI runs. Checkpointing currently requires
the thread cwd itself to be an approved subvolume root. Writable clones reject
nested subvolumes/mounts and external/shared Git metadata. Explicit checkpoint
recovery creates a new workspace; automatic in-place restore is not provided.

## Doctor, rollback and session-only opt-out

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root doctor --strict
ai-fence-cli codex-desktop --install-root /absolute/install/root doctor --thread-id THREAD_ID
ai-fence-cli ws ls
ai-fence-cli ws rm feature-a  # preview only
AI_FENCE_SNAPSHOTS=off /absolute/install/root/bin/codex app-server
AI_FENCE_CODEX_DESKTOP=0 /absolute/install/root/bin/codex app-server
```

Doctor emits only metadata (`--strict` exits nonzero for unhealthy integrity/route checks): profile, selected home/effective SQLite location,
saved and process-selected providers, logical/physical cwd, checkpoint mode,
owned file integrity, process/start identity, exact owning endpoint, listener
ownership and TCP liveness. `--endpoint http://127.0.0.1:PORT/v1` checks a supplied
route but does not claim ownership without a launch receipt. Startup success and
TCP liveness do not prove a successful model response. `--thread-id` distinguishes
workspace registration from native thread indexing. Check selected state store,
provider/account, cwd/aliases and archival independently. `resume --all` removes
cwd filtering only within the selected native store; it cannot find another
store's conversations. Newer options such as `tui.resume_cwd` are not assumed.

`AI_FENCE_CODEX_DESKTOP=0` selects native OpenAI routing for that invocation and
retains the same state home; it writes no persistent bypass marker. With a separate
native credential owner, the stdio auth bridge remains in memory so this invocation
still uses that owner's login instead of stale history-profile credentials. No
Fence proxy is started, and doctor labels the launch `native-stdio`/`fenced: false`.
Shared-owner TUI remains unsupported, including opt-out; use the ordinary native
CLI entry point or explicitly select profile auth during setup. For a Windows
session use `codex-session.ps1 -Mode Native` when ready to quit the prior app.
Close that session and launch fenced mode to return. `rollback` switches to the
previous verified installed release without restoring history/filesystem state;
running processes are untouched. `uninstall` previews owned shim removal;
`uninstall --apply` refuses active launches or changed owned files, removes only
owned dispatch commands, and retains native Codex, state, records and releases.

## Validation and limits

The public source workspace includes synthetic desktop setup/workspace tests:

```bash
python3 -B -m unittest discover -s crates/ai-fence-cli-core/tests/desktop -v
```

To include the native protocol fixture, provide an official CLI binary and the
real native Codex executable through `AF_TEST_CLI` and `AF_TEST_NATIVE`.
The native fixture uses temporary state and fake credentials, performs no model
inference and never installs into the running profile. The complete distributed
CLI composes this public launcher with the proprietary processing engine; this
public source workspace alone does not build that executable.

The real-binary test exercises setup/idempotence, two concurrent owning proxies,
configuration after peer exit, an in-flight upgrade, stdin EOF, SIGTERM,
listener ownership and absence of orphan listeners. It uses isolated homes and
fixture auth, initializes app-servers, reads configuration and registers a new
thread without sending model inference.
Rust synthetic fixtures cover native inter-chat named standalone function
outputs without `call_id`, paired output metadata, redaction, resumed history,
queued follow-ups and provenance. HTTP and WebSocket local validation errors
carry `ai_fence_local_validation_error`, stage/origin, `upstream_called: false`
and `retryable: false`. Regressions verify zero upstream calls on rejection,
accept valid native deliveries and replay on the same WebSocket after rejection,
and preserve the structured HTTP 400 through the local proxy without retries. Standalone named
outputs currently support the observed string-output format. Lossy cross-format
routing is rejected instead of changing them into user instructions.

Native protocol checks used Codex 0.162.1 and are version specific; Windows UI discovery, real subscription refresh,
real model inference and writable Btrfs cloning require platform validation
before production adoption. No real credentials, running production artifacts,
recovery relays, user histories or Windows processes are changed by the tests.

Official interfaces: [app-server](https://developers.openai.com/codex/app-server),
[CLI/resume](https://learn.chatgpt.com/docs/cli/reference),
[state and environment](https://learn.chatgpt.com/docs/config-file/environment-variables).
