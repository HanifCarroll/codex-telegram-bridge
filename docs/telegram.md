# Telegram Setup

Telegram is the bridge-owned remote control surface for Codex.

The intended behavior:

- no Telegram notifications while you are at your computer
- `away on` enables outbound Telegram notifications for new Codex updates
- `away off` disables outbound Telegram notifications
- replies to bridge-sent Telegram messages are routed back to the originating Codex thread
- approval messages include `Approve` and `Deny` buttons
- slash commands can toggle away mode, inspect state, and start new Codex threads from Telegram

Hermes can still control Codex through MCP when you ask it to, but Hermes and MCP do not own Telegram delivery.

## Install

```bash
cargo install --path .
codex-telegram-bridge doctor
```

## One Command Setup

Create a Telegram bot with `@BotFather`, then run:

```bash
codex-telegram-bridge setup --bot-token <telegram-bot-token>
```

When prompted by the JSON output, send `/start` to the bot from the chat that should receive Codex updates. The bridge records the chat id and user id in `~/.codex-telegram-bridge/config.json` with user-only file permissions.

For non-interactive setup:

```bash
codex-telegram-bridge setup \
  --bot-token <telegram-bot-token> \
  --chat-id <telegram-chat-id> \
  --allowed-user-id <telegram-user-id>
```

The token can also come from `TELEGRAM_BOT_TOKEN`.

## Lower-Level Telegram Setup

If you want to configure Telegram without installing or starting the daemon:

```bash
codex-telegram-bridge telegram setup --bot-token <telegram-bot-token>
codex-telegram-bridge telegram setup \
  --bot-token <telegram-bot-token> \
  --chat-id <telegram-chat-id> \
  --allowed-user-id <telegram-user-id>
```

`telegram setup` clears any existing Telegram webhook for that bot token because the daemon uses local long polling for replies and button callbacks.

Setup also registers the bot command menu with Telegram. The daemon re-registers the same command menu on startup as a best-effort repair path.

## Test Delivery

```bash
codex-telegram-bridge telegram test --message "Codex bridge is ready"
```

## Run The Daemon

```bash
codex-telegram-bridge daemon install --dry-run
codex-telegram-bridge daemon install
codex-telegram-bridge daemon start
codex-telegram-bridge daemon status
```

For foreground testing:

```bash
codex-telegram-bridge daemon run --once
codex-telegram-bridge daemon run
```

## Presence Gate

```bash
codex-telegram-bridge away status
codex-telegram-bridge away on
codex-telegram-bridge away off
```

`away on` starts a new away session. The daemon only sends events observed after that session started, so old waiting threads do not flood Telegram when you leave. `away off` clears pending outbound notifications so delayed retries do not notify you after you return.

Inbound Telegram replies and button callbacks are processed whenever the daemon is running. The away state only controls outbound notifications.

## Reply Flow

When Codex needs attention and you are away, the daemon sends a Telegram message. Reply directly to that Telegram message with the exact text you want sent to Codex. The daemon reads Telegram updates by long polling, looks up the original Telegram message id in SQLite, and calls the local Codex thread reply flow.

For approval prompts, use the `Approve` or `Deny` buttons. The callback data contains only an opaque route id; the thread id stays in the local SQLite route table.

## Notification Format

Bridge notifications are designed to feel like Codex is speaking directly in Telegram:

```text
✅ Codex finished
🧵 Thread name

<Codex final answer verbatim>

💬 To continue this thread, use Telegram's Reply action on this message.
```

Thread ids are intentionally hidden. The bridge still stores the Telegram message id to Codex thread id route locally, so replying to the specific Telegram message is enough.

Approval messages use a compact approval header and a button-aware footer:

```text
🔐 Codex needs approval
🧵 Thread name

<approval prompt verbatim>

Use the buttons below, or use Telegram's Reply action on this message.
```

Codex app directives such as `::inbox-item{...}` are not stripped or summarized. They are part of the Codex answer body, so Telegram receives the same final answer that appears in the Codex app.

## Slash Commands

The bridge accepts these standalone Telegram commands from the paired chat and allowed user:

- `/start`: pair during setup, or show help after setup
- `/help`: show command help
- `/away_on`: enable away mode notifications
- `/away_off`: disable away mode notifications and clear pending outbound notifications
- `/status`: show away mode, pending delivery count, and waiting thread count
- `/new_thread <prompt>`: create a new Codex thread and send the prompt immediately
- `/new_thread`: ask for a prompt; use Telegram's Reply action on the prompt message to create the thread
- `/inbox`: show actionable cached Codex inbox rows
- `/waiting`: show cached threads waiting for user input or approval
- `/recent`: show recent cached Codex threads
- `/settings`: show current Telegram bridge settings

Commands only run as standalone messages. If a message is a Telegram reply to a Codex notification, the text is sent back to that Codex thread verbatim, even if it starts with `/`.

## Token Ownership

Use a Telegram bot token dedicated to this bridge. Telegram update delivery should have one owner. If the same token is already used by another process or webhook, direct reply routing becomes ambiguous and unreliable.

## Status And Disable

```bash
codex-telegram-bridge telegram status
codex-telegram-bridge telegram disable --dry-run
codex-telegram-bridge telegram disable
```

Disabling Telegram removes the daemon config file because Telegram is the only supported notification transport.

## Security Notes

- The Telegram bot token is stored only in the local daemon config file.
- Command output redacts the bot token.
- `allowed_user_id` restricts inbound Telegram replies and button callbacks to the paired user.
- MCP remains local stdio only; do not expose it on an untrusted remote transport.
