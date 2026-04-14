# Security Policy

## Reporting a vulnerability

Please do not open a public GitHub issue for security-sensitive problems.

Instead, report vulnerabilities privately to the maintainer with:
- a clear description of the issue
- impact assessment
- reproduction steps or proof of concept
- any suggested mitigation

## Scope notes

This project can:
- execute local hook commands via `watch --exec`
- read and persist local Codex thread metadata/state
- send notifications through user-supplied integrations/examples

Please treat local environment details, thread content, and secrets used in downstream hooks as potentially sensitive.
