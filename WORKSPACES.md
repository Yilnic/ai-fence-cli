# Named workspaces for parallel agents

The named-workspace interface starts with CLI v0.1.8. Use matching
workspace-aware CLI and desktop adapter builds.

Give each parallel task a persistent **writable workspace**. A name selects
the same directory on every launch, including its Git index, commits,
uncommitted changes and untracked files. Ordinary resume never resets or
recreates that directory. Read-only recovery checkpoints are a separate,
optional feature and default to **off**.

## Why we recommend Btrfs

Btrfs creates writable copy-on-write snapshots quickly without initially
copying every file. This suits large repositories, dependency trees and
several agents starting from the same current working state. Each workspace
has its own files and independent Git metadata, while unchanged data blocks
are shared physically. Changes in one workspace do not change another.
The workspace name does not rename a Git branch; each copy starts with the
source's branch and has independent refs and index.

The project must be a user-owned **Btrfs subvolume root**, and its workspace
store must be on the same Btrfs filesystem. A normal directory on Btrfs is
not itself a subvolume. Linux 4.18+ and `btrfs-progs` are required. Nested
subvolumes, nested mounts, submodules, nested repositories and external/shared
Git metadata are refused rather than silently producing incomplete isolation.

A linked Git worktree usually has a `.git` file pointing outside its directory.
Snapshotting it would retain shared Git metadata. Start from a standalone Git
checkout whose `.git` directory and object database are inside the project.

WSL distributions usually use ext4. Creating another directory there does not
enable this feature: first make a Btrfs volume available inside WSL. AI Fence
does not format drives, convert live directories, mount volumes or elevate.
Native Windows Codex connections remain available separately.

For a **new** project on an existing, user-owned Btrfs mount, for example:

```bash
btrfs subvolume create /mnt/btrfs/projects/my-project
git clone REPOSITORY_URL /mnt/btrfs/projects/my-project
cd /mnt/btrfs/projects/my-project
ai-fence-cli ws init --directory /mnt/btrfs/ai-fence-workspaces
```

Use your actual mounted paths. For an existing project, preserve its original
checkout and all uncommitted/untracked files while preparing a standalone
checkout in a subvolume; changing a live agent's cwd is not part of setup.
`init` chooses storage for new projects. A project remembers a custom store
selected with `create --directory`; existing workspaces are never moved.
Missing configured storage must be reconnected, not replaced with an empty
directory on the underlying filesystem.

Copy-on-write is initially cheap, not free forever. Retained changed blocks,
new files and Git objects consume space. Creation checks a free-space floor
(`AI_FENCE_WORKSPACE_MIN_FREE_BYTES`, default 1 GiB); it does not automatically
delete work. These copies live on the same filesystem and are not independent
backups. Workspace selection changes where a command starts; its permissions
and sandbox policy remain the command's own.

## Shell setup and short commands

The standard command is `ai-fence-cli workspace`, with aliases `ws` and
`workspaces`. `fw` means **Fence workspace** and manages workspaces.
`fws` means **Fence workspace switch** and only changes your current directory.
Workspace names never select helper commands: `fws ls` enters a workspace named
`ls`; `fw ls` lists workspaces.

Official `setup` and `install-self` install the helpers and initialize user shell
profiles. Open a new terminal afterwards: a child process cannot initialize an
already-running parent shell. `install-self --no-shell-init` installs helper files
without editing profiles; `--no-wrappers` skips the helpers too. Channel installs
use distinct names, for example `fw-dev` and `fws-dev`.

To initialize or repair a particular shell explicitly:

```bash
ai-fence-cli ws shell-setup --shell bash
ai-fence-cli ws shell-setup --shell zsh
```

`--profile PATH` selects a custom profile; `--directory PATH` selects helper
storage. Setup replaces only its marked block and preserves other profile text.
PowerShell UTF-8/UTF-16 profiles retain their encoding and BOM. Generated
PowerShell helpers use UTF-8 with a BOM for Windows PowerShell 5.1. Native
output decoding and CMD's code page switch to UTF-8 only during a command,
then return to their previous settings, including when a workspace switch fails.
To initialize just the current Bash/zsh session:

```bash
eval "$(ai-fence-cli ws shell-init)"
```

The generated functions are inspectable with `ai-fence-cli ws shell-init
--shell bash`. Bash includes workspace-name completion; zsh has navigation and
management helpers. PowerShell on Linux is supported through `--shell powershell`.

### Windows: PowerShell, CMD and Git Bash

Install the Windows executable, then run `ai-fence-cli.exe install-self` (also
performed by `setup`). It installs helpers for Windows PowerShell, PowerShell 7,
CMD and Git Bash. Native Windows agent commands remain available.
PowerShell initialization assigns the `fw` shortcut to Fence workspace;
use the full `Format-Wide` command for PowerShell's former built-in `fw` alias.

Windows helpers run management and agent commands in WSL. Install the Linux CLI
in the selected distribution first, normally at `~/.local/bin/ai-fence-cli`.
They use the default distribution, or the distribution owning the current WSL
UNC directory. Configure an explicit choice when needed:

```powershell
$env:AI_FENCE_WORKSPACE_WSL_DISTRO = 'MyDistribution'
$env:AI_FENCE_WORKSPACE_WSL_CLI = '/absolute/path/to/ai-fence-cli'
fw create feature-a --from /absolute/linux/project
fws feature-a --project /absolute/linux/project
fw run feature-a -- codex-fenced --yolo
```

These variables choose routing, not an account or a fresh history store. A cwd
belonging to another explicitly selected distribution is rejected. Use relative
paths or Linux paths for workspace options such as `--from` and `--project`.

`fws` uses `Set-Location -LiteralPath` in PowerShell, `cd` in Git Bash, and
`pushd` in CMD because CMD cannot `cd` directly into a UNC directory. Use `popd`
to return in CMD. Generated CMD files are installed beside the CLI; setup adds
that directory to the Windows user PATH without removing other entries.
`--no-shell-init` leaves PATH and profiles untouched. From a batch file use
`call fws NAME` / `call fw ...`.
No global CMD AutoRun registry entry is installed. The current shell keeps its
own platform: use `fw run NAME -- COMMAND...` to run Linux tools from Windows.

For a custom PowerShell profile:

```powershell
ai-fence-cli.exe ws shell-setup --shell powershell --profile $PROFILE
```

Btrfs is still required inside WSL. Windows helpers do not add Btrfs support to
NTFS. Windows commands can inspect the workspace through its WSL UNC path.

## Daily use

From your project, create and enter a workspace once:

```bash
fw create feature-a
fws feature-a
codex-fenced --yolo
```

In another terminal in the original project or one of its workspaces:

```bash
fw create feature-b
fws feature-b
codex-fenced --yolo
```

Return tomorrow with the same name:

```bash
fws feature-a
codex-fenced
```

You can also run a foreground command without changing the calling shell:

```bash
fw run feature-a -- codex-fenced --yolo
```

Different names run independently. A workspace lease rejects another
independent launch using the same workspace while its owner is active.
Nested wrappers/commands in one owning process tree can borrow that lease.
`create` defaults to the original project even from inside another workspace.
Use `fw create feature-b --from .` at a workspace root to deliberately fork its
current state instead.

Names are scoped to the original project, including filesystem aliases.
Use `fw ls --all` to see other projects, or `fws feature-a --project
/absolute/original/project` when selecting from elsewhere.

## Manual cd and existing aliases

No shell helper is required:

```bash
ai-fence-cli ws create feature-a --from .
cd "$(ai-fence-cli ws path feature-a)"
codex-fenced --yolo
```

You can copy the printed path and use ordinary `cd`, your file manager or your
existing shell navigation tools. `codex-fenced` discovers the owned workspace
marker and acquires its lease automatically, including from subdirectories.
Keep the marker and registry: they identify an existing workspace without
turning a run ID, proxy port or path alias into a new workspace.

Custom shell aliases and functions work normally after `fws NAME` or `cd`.
For commands launched through the generic runner, use executable commands or
functions available through an interactive workspace shell:

```bash
ai-fence-cli ws run feature-a -- codex-fenced --yolo
ai-fence-cli ws shell feature-a
```

Everything after `--` is passed unchanged. Fence does not consume the agent's
flags. The runner preserves stdio, signals and the command's exit status.
Use foreground commands, or keep the workspace shell open while using
background tools whose lifetime should keep the workspace leased.

## Codex desktop and conversation history

Open the named workspace's path as a project in the configured WSL desktop
connection; use `fw path feature-a` to obtain it. The
[desktop adapter](CODEX_DESKTOP_WSL.md) shares the same registry and observes
actual thread start/resume/fork responses. It reserves the workspace before
forwarding work and links the returned native thread ID afterwards.

Resume reuses the current workspace and saved subdirectory. Missing or replaced
directories produce an explicit recovery error. Thread and filesystem identity
remain stable across proxy ports, process restarts and logical path aliases.
The adapter releases the last thread's lease when Codex confirms
`thread/closed` or `notLoaded`, or when its connection shuts down. Unsubscribe
alone can leave native work running during a grace period, so it does not
unlock immediately. This follows the
[official app-server lifecycle](https://learn.chatgpt.com/docs/app-server).

Workspace choice does not choose an account, provider, profile or `CODEX_HOME`.
Configure the desktop adapter to use the same existing state store as your
CLI profile if you want the same history in both entry points. Separate stores
still have separate resume lists; `resume --all` only removes cwd filtering
within the selected store. No credentials, SQLite stores or transcripts are
merged or rewritten by workspace management.

Interactive CLI pickers do not expose thread IDs to their parent. The launcher
records the selected native state-home identity and protects that workspace
from ordinary removal without inventing thread IDs. When such a thread is
opened in the desktop, the adapter uses native metadata to register its
workspace. Known registered IDs must be resumed from their saved workspace;
enter it with `fws NAME` first for a CLI resume.

## Inspect, review and bring changes back

```bash
fw ls
fw status
fw diff feature-a
fw run feature-a -- git status
```

`ls` shows the path, active/idle/missing state, protected work and chat links.
`status` includes physical/project identity and PID/start metadata, with no
credentials or chat contents. `CHANGES` includes inherited or new untracked
and ignored files, uncommitted changes, and commits beyond the creation HEAD.
`+CLI` indicates native CLI store usage even if individual threads are not indexed.

`diff` compares tracked files with the creation HEAD and lists untracked names
separately; those file contents are not included in the patch. Review and commit
the desired work in its workspace, then use ordinary local Git fetch and
cherry-pick to bring the reviewed commits back:

```bash
cd /absolute/original/project
git fetch "$(fw path feature-a)" HEAD
# Cherry-pick the reviewed commit(s), using normal Git conflict handling.
```

There is no automatic commit, merge, reset or copy back into the original project.

## Removal and explicit recovery

```bash
fw rm feature-a                 # preview only
fw rm feature-a --apply         # remove only if eligible
fw pin feature-a               # retain explicitly; clear with --unpin
```

Active and pinned workspaces are always protected. After reviewing the preview,
`--discard-changes` explicitly permits discarding uncommitted/untracked/ignored
files and workspace commits. `--detach-chats` acknowledges loss of the linked
filesystem workspace while retaining native history and the removal record.
These flags do not bypass an active lease. No recurring cleanup is installed.

Closing an agent retains its writable workspace. Names are reused until you
explicitly remove them. Explicit creation after removal gives a new identity;
old chats continue to report the removed workspace, even if its name is reused.

Read-only checkpoints are optional and follow their own approved-root policy.
An explicit `snapshot create` command is opt-in when the environment policy is
unset; explicit `AI_FENCE_SNAPSHOTS=off` blocks it. See the checkpoint helper
documentation or `ai-fence-cli snapshot --help`.
To recover a complete single-member checkpoint into a new writable workspace:

```bash
fw recover rescue --checkpoint /absolute/checkpoint/manifest.json
# Inspect the preview; then repeat with --apply.
fw recover rescue --checkpoint /absolute/checkpoint/manifest.json --apply
fws rescue
```

Recovery validates provenance, member UUID, read-only state and Git independence.
It preserves existing workspaces, auth and history. Start or fork a conversation
in the recovered workspace; it does not silently reassign an old conversation
or restore files in place. Checkpoint references and conversation history do
not automatically describe the same instant.

The public standalone `ai-fence-workspace` helper can be built with
`cargo build -p ai-fence-cli-core --bin ai-fence-workspace` in this public
workspace. It provides the same management engine; lease-aware `codex-fenced`
and desktop launching require the matching complete AI Fence CLI distribution.
