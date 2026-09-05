# Security Policy

## Supported versions

Security fixes are applied to the latest published AI Fence CLI release. Update
to the latest version before reporting a problem that may already be fixed.

## Reporting a vulnerability

Use
[GitHub private vulnerability reporting](https://github.com/matthid/ai-fence-cli/security/advisories/new).
Do not open a public GitHub issue, GitLab issue, discussion, or merge request for
a suspected vulnerability.

Include the affected version and platform, impact, minimal reproduction, and
any relevant sanitized diagnostics. Do not send live provider credentials or
customer data. The maintainers may provide a private channel if additional
sensitive evidence is required.

The release owner must enable private vulnerability reporting and test the link
while logged out before making the repository public. If the private report
form is unavailable, do not publish sensitive details; wait for the release
owner to restore the private reporting path.

## Scope

This repository contains the MIT-licensed launcher, transport, configuration,
contract, and model-metadata crates. Reports about the official binary's
licensed processing engine or a managed AI Fence deployment can still start
through the private report form and will be moved to the appropriate private
tracker.
