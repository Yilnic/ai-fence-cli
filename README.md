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

The installer also provides `claude-fenced` and `interpreter-fenced`. The
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

## Source boundary

The proprietary request-processing engine is intentionally not part of this
repository. The official binary composes this launcher with redaction,
normalization, provider compatibility, policy, and storage code distributed
under the AI Fence product license. This workspace builds and tests the public
MIT-licensed libraries; it intentionally does not build the official product
executable. Free to use does not mean that every component of the official
binary is open source.

## Development

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
