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

## Token rotation

If a Telegram bot token is exposed or should be replaced:

1. rotate or revoke it with `@BotFather`
2. remove the old local config with `codex-telegram-bridge telegram disable` if you want the bridge offline immediately
3. rerun `codex-telegram-bridge setup --bot-token <new-token>` or `codex-telegram-bridge telegram setup --bot-token <new-token>`
4. restart the daemon if the bridge was already running

The bridge stores only the active token in `~/.codex-telegram-bridge/config.json`. Old token values are not retained by the bridge once the config file is replaced.

## Local data retention and deletion

The bridge stores local data under `~/.codex-telegram-bridge/`. This can include:

- `config.json` with Telegram configuration and registered projects
- `state.db` with cached thread metadata, cached previews, Telegram route ids, inbound update dedupe state, and related bridge bookkeeping
- `daemon.out.log` and `daemon.err.log`

This data is retained locally until you delete it. To remove the Telegram configuration only, run:

```bash
codex-telegram-bridge telegram disable
```

To remove all bridge-local state, stop the daemon and delete `~/.codex-telegram-bridge/`.
