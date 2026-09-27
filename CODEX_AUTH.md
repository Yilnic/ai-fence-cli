# Codex authentication through AI Fence

If an API-key-backed `codex-fenced` run can send model requests but
`codex-apps` MCP or `/usage` asks you to log in, successful inference alone
does **not** establish a native ChatGPT session. An auth-pool rental instead
provides a temporary native Codex credential for that run.

## Which identity is in use?

| Launch authentication | What the launcher configures | Native account features |
| --- | --- | --- |
| Gateway/provider API key | Custom provider `env_key` | Does not create a ChatGPT session. |
| Auth-pool rental | A rented `auth.json` is available in the managed Codex home for the run, then removed when the rental is returned. | Account features may still depend on Codex and the rented account. |
| Your local subscription | `requires_openai_auth = true`; Codex owns native login/refresh | Uses your genuine OpenAI login; feature access still depends on Codex version, account and workspace policy. |

The rental's native credential is temporary. AI Fence writes the rented
`auth.json` before starting Codex and removes it after returning the rental.
Recent Codex versions can reuse a
background app-server daemon whose account state was loaded after the previous
rental ended. The interactive TUI then shows a login chooser even though the
new run has a valid credential. For rented sessions, AI Fence starts Codex with
`--no-daemon` when that flag is supported, so each run reads its own credential.
Provider API keys still do not create a native ChatGPT session. Do not set
`requires_openai_auth = true` on an API-key profile as a workaround; that
changes the authentication source rather than granting account access.

Codex still documents `CODEX_HOME` as its state directory and
`CODEX_HOME/auth.json` as the file credential store. Its CLI already starts a
local daemon by default; the experimental label applies to directly running
and connecting to the app-server interface. A future AI Fence integration
could give each rental a separate `CODEX_HOME` and daemon, then stop that daemon
before returning the rental. This needs explicit lifecycle and crash-recovery
handling so a returned credential cannot remain active in daemon memory.

The app-server API also exposes `account/read` and an external-token login
mode, but the latter is experimental and requires the host to already own the
account's authentication lifecycle and answer token-refresh requests. It does
not document handing a Codex-managed refresh token to another refresher. For
rented `auth.json` bundles, Codex should refresh its own credential; AI Fence
preserves the updated file when returning the rental. See the
[Codex configuration guide](https://learn.chatgpt.com/docs/config-file/config-advanced)
and [app-server auth API](https://learn.chatgpt.com/docs/app-server).

## Safe next steps

1. Identify the selected AI Fence profile and its configured lane. Named
   profiles have separate native `CODEX_HOME` directories; checking the default
   `~/.codex` says nothing about another profile. Do not print auth files or
   provider-token command output when collecting diagnostics.
2. For API-key or pooled model access, continue using the working model lane.
   Use AI Fence's usage reporting for Fence-accounted requests. A native
   `/usage` result is not a report of your Fence budget or shared-pool quota.
   API-key access alone does not grant ChatGPT account features; a pooled
   account's feature access depends on that account and Codex. Do not
   repeatedly re-rent credentials or re-run setup to fix an account feature.
3. If you need ChatGPT account features, use a **separate personal Codex home**
   and your own authorized account. Native commands (not `codex-fenced`) are:

   ```sh
   CODEX_HOME="$HOME/.codex-personal" codex login
   CODEX_HOME="$HOME/.codex-personal" codex login status
   ```

   These commands authenticate with OpenAI; they do not change a Fence rental
   into a personal subscription. Running native Codex there without a Fence
   provider configuration is outside Fence routing, inspection and accounting.
   Obtain any required organization approval first. For personal subscription
   traffic through Fence, use its existing local-subscription setup with a
   dedicated profile instead of converting a pooled profile. Do not mix a
   personal account's app access with another account's rental model traffic.
4. If genuine personal login still fails, record the Codex version, selected
   lane, sanitized error, and whether login status was checked in the same
   home. Check account/workspace permissions and official Codex guidance.
   Never attach `auth.json`, bearer tokens, or rental recovery files to a bug.

The launcher defaults native credential storage to `file` unless a template
specifies otherwise. File credentials live under `CODEX_HOME`; keyring-backed
credentials are not imported merely by copying `auth.json`. Profile runtime
configuration is generated on launch, so manually editing that generated
file is not a durable repair.

## Evidence and verification limits

[OpenAI authentication documentation](https://developers.openai.com/codex/auth/)
(read 2026-09-27) distinguishes OpenAI authentication from custom-provider
`env_key` authentication, documents `codex login` / `codex login status`, and
explains file/keyring storage. It notes that API-key access can lack features
requiring ChatGPT workspace or cloud access. It does not establish that AI
Fence's provider API key grants `codex-apps` or `/usage` access.

No claim is made that every Codex version exposes these surfaces identically,
or that personal login guarantees app entitlement. The auth-pool path was
checked with Codex 0.157.1: the temporary credential was available during the
run, and the interactive TUI reached its normal prompt. MCP and `/usage`
entitlements were not checked.
