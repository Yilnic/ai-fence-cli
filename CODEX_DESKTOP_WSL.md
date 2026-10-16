# Codex desktop and native WSL setup

This opt-in target is bundled in `ai-fence-cli`; it requires Linux/WSL and
Python 3.11+. Install the CLI normally, then run the target below. It preserves
the genuine native executable and explicitly selects a durable Codex state
store. Setup does not activate a running desktop connection or replace its
proxy. Native Windows connections remain available.

```bash
ai-fence-cli codex-desktop --install-root "$HOME/.local/share/ai-fence-codex-desktop" setup \
  --native-codex /absolute/path/to/genuine/codex \
  --state-home /absolute/path/to/chosen/codex/state \
  --profile personal
```

Choose the existing store containing the conversations you want to resume.
`CODEX_HOME` contains native auth, configuration, sessions and indexes. Existing
AI Fence stores usually live under
`$AI_FENCE_HOME/profiles/PROFILE/codex/state`; the native default is `~/.codex`.
Setup requires an explicit store so it cannot silently start a fresh history.
The same adapter handles CLI and desktop entry points with that store. Its
profile default is `AI_FENCE_CODEX_PROFILE`, then `AI_FENCE_PROFILE`, then
`default`; selecting a profile never merges stores or accounts.

The native path is resolved to its physical executable. AI Fence never renames
or overwrites it. A native upgrade can select the new genuine executable by
rerunning setup with the same state home and profile. Existing app-servers keep
their executable, selected release, state home and process-owned endpoint.
Setup checks owned file hashes and refuses edited/unowned commands. Releases
are content-addressed and retained for rollback. Repeating identical setup is
idempotent. The payload includes the CLI, adapter, runtime helper, snapshot
command dispatcher and a Windows session launcher; Python is a prerequisite.

Authentication stays native. Existing managed auth always wins. If seeding a
new managed store is necessary, explicitly add `--seed-home /same/account/home`.
It copies an absent `auth.json` under a lock; it never links credentials or
replaces an existing bundle. Native Codex owns refresh and atomic auth updates.
Alternatively run `INSTALL_ROOT/bin/codex login` against the chosen store.
Config is preserved: provider, Responses transport and owning loopback endpoint
are process overrides. Desktop launches do not write ephemeral routes into the
shared `config.toml`. Concurrent profiles can use separate install roots/stores;
concurrent launches of one installation receive independent proxies.

## Connect the Windows desktop

Use the installed `INSTALL_ROOT/bin/codex` for the WSL connection. Discovery is
version dependent: setup deliberately does not edit Windows configuration,
change default CLI aliases, or download a native CLI over the adapter.

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

## Checkpoints and persistent workspaces

Default mode keeps the original cwd and takes **read-only checkpoints** of an
explicitly approved current project. Add policy during setup:

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root setup \
  --native-codex /absolute/native/codex --state-home /absolute/state --profile personal \
  --snapshots auto --approve-root /absolute/project/subvolume \
  --snapshot-directory /absolute/same-btrfs/checkpoints
```

Repeat `--approve-root` for additional exact roots. Ordinary directories and
unsupported filesystems produce an unavailable outcome. A `required` checkpoint
failure closes the owning stdio connection before its thread response is
forwarded; this strict mode can affect other threads on that same connection. `off`
disables checkpoint creation without disabling Fence. Native app-server thread
start/resume/fork responses and `thread/started` notifications drive registration,
including multiple chats in a long-lived server. Startup IDs never choose the
chat's workspace. Each registered checkpoint is pinned against ordinary prune.
Nested checkpoints are sequential manifest sets; pause writers for consistency.
Changed retained blocks consume space. Checkpoints are not independent backups.

The installed `ai-fence-snapshot` dispatcher exposes status, inspect, create,
list, pin and prune through the bundled CLI. `AI_FENCE_SNAPSHOT_MIN_FREE_BYTES`
controls the free-space floor. Prune defaults to preview, caps each invocation at 32 sets (`--limit`), and protects pinned,
incomplete, recent and newest sets. Use `ai-fence-snapshot --help` to inspect checkpoint commands and options.
Workspace cleanup is a bounded metadata preview and preserves all chat-linked,
active, dirty, pinned and unowned workspaces. Old runtime releases are retained;
this version does not automatically delete releases or writable workspaces.

Writable isolation is explicit and persistent, one workspace per thread:

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root workspace create \
  --thread-id THREAD_ID --project /approved/project --directory /same-btrfs/workspaces
# Inspect the preview, then repeat with --apply.
```

Creation copies the current writable Btrfs project, including dirty and
untracked files. It refuses external/shared Git metadata, object alternates,
nested Git repositories/submodules and nested subvolumes. It checks Git metadata
independence again after cloning. Use a supported native Git worktree or
explicitly adopted shared workspace for those layouts. Ordinary resume reuses
the current workspace and preserves subsequent changes. Filesystem aliases are
matched through canonical paths, inode/device and Btrfs subvolume identity.
An active lease prevents a second process from resuming the same registered
thread. Different threads can deliberately share the original project.

Records under `CODEX_HOME/ai-fence-workspaces` are versioned metadata: thread,
profile/store, logical root, physical project/workspace identity, checkpoint
references, pin state and process/start/boot leases. They contain no messages or
credentials. Missing/replaced workspaces fail explicitly. Resume never restores
a pre-launch checkpoint. To adopt an existing thread without moving history:

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root workspace adopt \
  --thread-id THREAD_ID --project /original/project [--workspace /chosen/shared/workspace]
# Preview first; --apply registers the explicit choice and preserves the old record.
```

Adoption uses native `thread/read` metadata and validates the original project;
it does not merge SQLite, rewrite transcripts, change provider metadata, or link
accounts. To recover to a new workspace from a selected complete single-member
checkpoint, use `workspace recover --thread-id ID --project ORIGINAL_ROOT
--checkpoint /set/manifest.json --directory /same-btrfs/workspaces`, inspect the
preview, then add `--apply`. It preserves the current workspace and previous
record. Nested checkpoint reconstruction and in-place restore are deliberately
manual in this version. Conversation and filesystem checkpoints have coordinated
references, but they are not automatically the same instant in time.

Interactive native CLI pickers do not expose new-thread notifications to their
parent. Explicit registered CLI resume IDs reattach and lease the saved workspace;
new picker-created CLI threads need explicit adoption before workspace management.
`resume --all`, explicit session IDs and native directory flags are forwarded.
A registered isolated workspace requires explicit adoption to change cwd.
Daemon and network-listener app-server transports are rejected because they can
outlive the owning proxy; the desktop stdio transport is supported. Setup probes
native `--no-daemon` support and uses it for fenced CLI runs when available.
Checkpoints currently require the thread cwd itself to be an approved subvolume
root; subdirectory cwd checkpointing needs explicit root-level helper commands.

## Doctor, rollback and session-only opt-out

```bash
ai-fence-cli codex-desktop --install-root /absolute/install/root doctor --strict
ai-fence-cli codex-desktop --install-root /absolute/install/root doctor --thread-id THREAD_ID
ai-fence-cli codex-desktop --install-root /absolute/install/root workspace cleanup --limit 32
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
retains the same state home; it writes no persistent bypass marker. For a Windows
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
