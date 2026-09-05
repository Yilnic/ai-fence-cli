# CLI Support

Use the public issue tracker for reproducible problems in the free CLI or the
public Rust workspace:

- GitHub: https://github.com/Yilnic/ai-fence-cli/issues
- GitLab: https://gitlab.com/matthid-ml/ai-fence-cli/-/issues

Before filing an issue, check the
[quickstart and troubleshooting guide](https://ai.yml.world/docs). Include:

- AI Fence CLI version from `ai-fence-cli --version`.
- Operating system and CPU architecture.
- Native tool and version, such as `codex --version` or `claude --version`.
- Whether the run used free standalone mode or an organization-managed gateway.
- Minimal synthetic reproduction steps, expected behavior, and actual behavior.
- Sanitized error text.

Do not attach credentials, provider auth files, request or response payloads,
proprietary source, protocol diffs, or sensitive logs. Replace private values
with clearly synthetic placeholders before posting.

For a managed or self-hosted gateway, contact the administrator that supplied
the deployment. Public CLI maintainers cannot inspect an organization's
identity provider, policy, credentials, capture, or retention configuration.

For vulnerabilities or sensitive reports, use the private path in
[SECURITY.md](SECURITY.md), not a public issue.
