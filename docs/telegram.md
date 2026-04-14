# Telegram Setup

Direct Telegram delivery is the primary product path for proactive Codex control.

The bridge owns the Telegram bot transport:

- `daemon run` watches Codex state and sends Telegram messages directly.
- The daemon stores Telegram `message_id -> thread_id` routes in SQLite.
- A Telegram reply to a bridge-sent message is sent back to the originating Codex thread.
- Approval notifications include `Approve` and `Deny` buttons that map to the same thread.

Hermes is still useful as an agent control plane through MCP, but Hermes does not need to own Telegram delivery for direct reply routing to work.

## Install

```bash
cargo install --path .
codex-hermes-bridge doctor
```

## Pair Telegram

Create a Telegram bot with `@BotFather`, then run:

```bash
codex-hermes-bridge telegram setup --bot-token <telegram-bot-token>
```

When prompted by the JSON output, send `/start` to the bot from the chat that should receive Codex updates. The bridge records the chat id and user id in `~/.codex-hermes-bridge/config.json` with user-only file permissions.

For non-interactive setup:

```bash
codex-hermes-bridge telegram setup \
  --bot-token <telegram-bot-token> \
  --chat-id <telegram-chat-id> \
  --allowed-user-id <telegram-user-id>
```

The token can also come from `TELEGRAM_BOT_TOKEN`.

## Run The Daemon

```bash
codex-hermes-bridge daemon install --dry-run
codex-hermes-bridge daemon install
codex-hermes-bridge daemon start
codex-hermes-bridge daemon status
```

For foreground testing:

```bash
codex-hermes-bridge daemon run --once
codex-hermes-bridge daemon run
```

## Reply Flow

When Codex needs attention, the daemon sends a Telegram message. Reply directly to that Telegram message with the exact text you want sent to Codex. The daemon reads Telegram updates by long polling, looks up the original Telegram message id in SQLite, and calls the local Codex thread reply flow.

For approval prompts, use the `Approve` or `Deny` buttons. The callback data contains only an opaque route id; the thread id stays in the local SQLite route table.

## Token Ownership

Use a Telegram bot token dedicated to this bridge. Telegram update delivery should have one owner. If the same token is already used by another process or webhook, direct reply routing becomes ambiguous and unreliable.

`telegram setup` clears an existing Telegram webhook before enabling local long polling for the bridge-owned bot.

## Status And Disable

```bash
codex-hermes-bridge telegram status
codex-hermes-bridge telegram disable --dry-run
codex-hermes-bridge telegram disable
```

## Security Notes

- The Telegram bot token is stored only in the local daemon config file.
- Command output redacts the bot token.
- `allowed_user_id` restricts inbound Telegram replies and button callbacks to the paired user.
- MCP remains local stdio only; do not expose it on an untrusted remote transport.
