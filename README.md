# AI Fence CLI

This repository contains the open launcher, local transport, configuration,
authentication, shared API contracts, and model metadata used by AI Fence.

## Looking for the free CLI?

The official CLI is free to use and needs no AI Fence account or backend in
standalone mode. On Linux x86-64 or an Apple Silicon Mac running macOS 12+,
with Codex or Claude Code already installed:

```sh
curl -fsSL https://ai.yml.world/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
codex-fenced
```

The installer also provides `claude-fenced`, `interpreter-fenced`, and
`openclaw-fenced`. Managed OpenClaw runs use an isolated native config with a
custom AI Fence provider; `agent --local` needs no Gateway or daemon. The
interactive `ai-fence-cli setup` flow can authenticate first, search the
managed model catalog, save several preferred models, and reuse those choices
in each agent's native model-selection surface. Open Interpreter uses
transport-aware harness defaults such as `kimi-code` over Chat and `zcode`
over Anthropic Messages.

Windows x86-64 is available as a
[manual download](https://ai.yml.world/download). The macOS preview is ad-hoc
signed rather than Developer ID signed and notarized. See the
[quickstart](https://ai.yml.world/docs) for provider auth, standalone data flow,
logging, and manual install details.

For model access that works while `codex-apps` or `/usage` requests login, see
[Codex native authentication limitations and safe next steps](CODEX_AUTH.md).

## Source boundary

The proprietary request-processing engine is intentionally not part of this
repository. The official binary composes this launcher with redaction,
normalization, provider compatibility, policy, and storage code distributed
under the AI Fence product license. This workspace builds and tests the public
MIT-licensed libraries; it intentionally does not build the official product
executable. Free to use does not mean that every component of the official
binary is open source.

## Development

The public workspace also contains `ai-fence-client` and the
`ai_fence_contract::application` managed-connection DTOs. The initial client
supports application-binding reconciliation, machine client-credentials
authentication and binding-scoped readiness. The additive
`ai_fence_contract::application_run` contract and client submit/by-key/by-ID methods
support bounded structured-text attempts and metadata reconciliation. These methods
do not establish that the deployed producer implements model runs.
Applications must require the relevant enforced workload capabilities before
dispatching inference; a provisioned connection alone is insufficient.

The unreleased `application_native` module provides bounded native function-turn
content and stateless history validation. It preserves messages, assistant phase,
encrypted reasoning, function argument strings and actual client tool outcomes.
Function schemas and explicit `strict` choices are kept unchanged. It supports
plain direct function calls; hosted tools, programs, async calls and named tool
namespaces are outside this mode. The codec does not execute or authorize tools.
`application_run::RunInput` carries either the legacy structured object or this
native input. Native requests explicitly select `input_kind: "native_functions"`
and `receipt_version: 2`; the shared client submits them through the common run
endpoint and checks returned native output against the original input. Receipt
v2 preserves every available token category as decimal strings, known finite USD
and explicit held/settled/reconciliation state. Legacy requests omit both selectors
and keep their original wire and digest. Structured requests may opt into v2
receipts independently. The producer's native runtime switch defaults off.
Declaring `NATIVE_FUNCTION_TURNS_V1` alone never establishes readiness; require the
enforced role-specific native, full-receipt and metadata-only capabilities.
Native/mixed bindings omit the global v1 claim so old clients fail closed.
Producer verification and actual consumer adoption remain release gates.

The unreleased `audio_transcription` content codec checks transcript structure
and source-relative segment timestamps, with bounded, duplicate-free response
parsing. It keeps reported duration independent from transcript validity and
measured source duration. Canonical PCM16 WAV preparation strips optional WAV
metadata and binds the audio hash, measured duration and transcription options
into an explicit `audio_transcription`/receipt-v3 request. `submit_transcription`
sends metadata and private audio in one bounded multipart submission and validates
source-relative output before returning it. Ordinary JSON `submit_run` refuses
audio metadata without an upload. Receipt v3 carries exact duration strings,
never token estimates. Require role-specific `audio_transcription_runs_v1`,
`duration_run_receipts_v3` and metadata-only capabilities. Native/audio/mixed
bindings suppress the global legacy v1 claim so old clients fail closed.
Diagnostics omit private content; applications retain their transcripts and
review decisions. Speech rollout remains off and live quality is unverified.

Managed backends receive `AI_FENCE_CONNECTION_MODE=managed` and the non-secret
`AI_FENCE_CONNECTION_JSON` descriptor, with `AI_FENCE_CLIENT_SECRET` injected
separately through the deployment secret store. The descriptor carries a secret
reference, never a secret value. Service credentials are opaque, bounded and
not serializable/debuggable. The transport refuses redirects and credential-bearing
URLs and returns sanitized errors. Cached service credentials are bound to their
Fence origin, app/stage/service binding and issuer/client/audience/scope. A mismatch
is rejected before any request, including private-content submission. Historical
receipt lookup can retain that identity across binding/profile revision updates.
The client never falls back to a direct provider.

Consumers can vendor just `crates/ai-fence-contract` and `crates/ai-fence-client`
with this workspace's license and an exact source revision. No request engine
or backend code is required. This first contract is an additive source checkpoint;
it does not establish that a deployed gateway supports application connections.
Include each crate's complete source and tests, including `application_native.rs`,
`application_native/tests.rs`, `audio_transcription.rs`,
`audio_transcription/tests.rs`, `audio_transcription/input.rs`,
`application_run/receipt_wire.rs` and the v2/v3 and client audio test
modules, rather than an older file allowlist. This unreleased source does not
change any consumer's accepted pin or establish a combined handoff.

Run requests use one canonical SHA256 helper (recursively sorted compact JSON,
excluding only dispatch key and digest), at most three explicit attempts, and
application API funding with metadata-only retention. The untrusted JSON parser
rejects duplicate/unknown fields and bounds the request to 1 MiB, schema to 128 KiB,
instructions to 32 KiB and prompt to 512 KiB. Models and provider credentials
remain operator-selected. Native requests carry caller-owned plain function
definitions; those definitions confer no permission to execute an action.
Operator profile/admission limits can be stricter.

Persist the operation, attempt, dispatch key and digest before submission. A
transport error may mean dispatch occurred: reconcile by key instead of creating
a new paid attempt. There are no automatic HTTP/model retries. GET and duplicate
POST return metadata only. A completed `not_retained` response provides its usage
receipt but cannot replay lost content; apps persist authoritative inline results
locally. Unknown token counts/prices remain null. Results and credentials are
omitted from debug output. Run responses are limited to 1 MiB and inline results
to 512 KiB. No application job queue, product-credit ledger or provider engine is
part of this client. Existing enforced-capability checks remain mandatory.

Administrators can permanently revoke a binding ID with `revoke_binding` and read
its stable `BindingRevocationReceipt` with `binding_revocation`. The request carries
only the binding identity; the management credential remains separate. A producer
may record revocation before registration, preventing a late create or enable from
restoring access. Repeating the same request recovers the same receipt after a lost
reply. Temporary `enabled: false` pause remains distinct from terminal revocation.
These client methods alone do not prove a deployed producer implements revocation.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked -- --test-threads=4
./scripts/check-boundary.sh
```

## Support and security

Read [SUPPORT.md](SUPPORT.md) before opening a reproducible CLI bug. Never put
provider tokens, request payloads, proprietary source, or sensitive logs in a
public issue.

Report vulnerabilities through
[GitHub private vulnerability reporting](https://github.com/Yilnic/ai-fence-cli/security/advisories/new)
and follow [SECURITY.md](SECURITY.md). Do not report a vulnerability in a public
GitHub issue or GitLab issue.

Changes are accepted in either public mirror. Maintainers import contributions
into the private integration repository, run its full compatibility suite, and
publish the resulting subtree back to both mirrors.

- GitHub: https://github.com/Yilnic/ai-fence-cli
- GitLab: https://gitlab.com/matthid-ml/ai-fence-cli

Public contribution commits retain their original author, message, and
timestamp. Product-to-public updates use a release-bot snapshot so private
repository metadata is not disclosed.


Historical accepted-v1 application requests and numeric receipts remain readable
with the current SDK after binding/profile revisions advance. Recover their
original dispatch key/digest or run ID through metadata GET; do not rewrite saved
requests to select receipt2, invent missing usage/settlement or replay lost results.
Receipt2 is an explicit choice for new requests in a reviewed source upgrade.
The original accepted2c-generated known/unknown fixtures and GET-only regression
are part of the complete client test source. Consumer pins remain unchanged.

Managed document requests explicitly select `structured_document` with receipt2.
The complete shared source includes `document_input` and SDK `submit_document`;
original PDF/PNG/JPEG bytes/SHA256 are bound separately from strict output metadata.
Use metadata GET with the original key/digest for recovery; never replay a lost
private result. This unreleased producer extension changes no consumer source pin.

### Codex desktop and native WSL

The distributed `ai-fence-cli` binary includes `codex-desktop setup`, an opt-in
installer for a supervised desktop stdio adapter and runtime helper. Supply
`--native-codex /absolute/native/codex` and `--state-home /absolute/existing/state`.
It retains native Codex and the selected history/auth store, uses process-owned
proxy endpoints, and supports doctor, session-only opt-out, previewed workspace
adoption/cleanup and release rollback. The public crate bundles launcher code;
the complete CLI still links the private engine. Windows desktop discovery is
version dependent and must be verified before switching a running connection.

See [Codex desktop and WSL setup](CODEX_DESKTOP_WSL.md) for installation, state
selection, checkpoints, workspaces, diagnostics and rollback commands.
