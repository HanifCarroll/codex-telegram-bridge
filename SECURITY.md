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
- store a local Telegram bot token for bridge-owned notification delivery
- send Telegram notifications only when the local away gate is enabled
- route Telegram replies and approval button callbacks back into local Codex threads

Please treat local environment details, thread content, Telegram bot tokens, and secrets used in downstream hooks as potentially sensitive.

Do not expose the MCP stdio server or daemon state directory to untrusted users. Use a Telegram bot token dedicated to this bridge, and set `--allowed-user-id` during setup when possible.
