# AI Fence CLI

This repository contains the open launcher, local transport, configuration,
authentication, shared API contracts, and model metadata used by AI Fence.

The proprietary request-processing engine is intentionally not part of this
repository. The official binary composes this launcher with redaction,
normalization, provider compatibility, policy, and storage code distributed
under the AI Fence product license. This workspace builds and tests the public
libraries; it intentionally does not build the official product executable.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked -- --test-threads=4
./scripts/check-boundary.sh
```

Changes are accepted in either public mirror. Maintainers import contributions
into the private integration repository, run its full compatibility suite, and
publish the resulting subtree back to both mirrors.

Public contribution commits retain their original author, message, and
timestamp. Product-to-public updates use a release-bot snapshot so private
repository metadata is not disclosed.
