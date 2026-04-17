# Telegram Setup

Telegram is the bridge-owned remote control surface for Codex.

The intended behavior:

- no Telegram notifications while you are at your computer
- `/away` starts or reuses the shared local Codex backend and enables outbound Telegram notifications
- `/back` disables outbound Telegram notifications
- replies to bridge-sent Telegram messages are routed back to the originating Codex thread
- approval messages include `Approve` and `Deny` buttons
- slash commands can toggle away mode, inspect state, pick a project, and start new Codex threads from Telegram

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

The token can also come from `TELEGRAM_BOT_TOKEN`. Setup stores the shared Codex backend URL as `codex.websocketUrl`; by default it is `ws://127.0.0.1:4500`.

For manual recovery or first-run audits, sample files live at:

- [examples/config.example.json](../examples/config.example.json)
- [examples/telegram.env.example](../examples/telegram.env.example)

The preferred setup path is still `setup` or `telegram setup`, because those flows also validate the token, clear old webhooks, and pair the target chat/user.

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

```text
/away
/repair
/back
/status
```

`/away` is the normal pre-departure command. It starts a new away session, ensures the shared local `codex app-server` is running, and makes replies, approvals, and new Telegram-created threads use that same backend. The daemon only sends events observed after that session started, so old waiting threads do not flood Telegram when you leave.

`/repair` is the recovery command. Use it if `/status` reports an unhealthy backend or replies stop reaching Codex. It restarts the shared backend and keeps remote mode on.

`/back` clears pending outbound notifications so delayed retries do not notify you after you return.

Inbound Telegram replies and button callbacks are processed whenever the daemon is running. The away state only controls outbound notifications.

## Reply Flow

When Codex needs attention and you are away, the daemon sends a Telegram message. Reply directly to that Telegram message with the exact text you want sent to Codex. The daemon reads Telegram updates by long polling, looks up the original Telegram message id in SQLite, and sends the reply through the shared live Codex backend.

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

App-only file reference tokens such as `[F:/.../README.md†L1-L24]` are compacted before delivery so Telegram sees readable refs like `README.md L1-L24` instead of raw app link payloads.

Approval messages use a compact approval header and a button-aware footer:

```text
🔐 Codex needs approval
🧵 Thread name

<approval prompt verbatim>

Use the buttons below, or use Telegram's Reply action on this message.
```

Codex app directives such as `::inbox-item{...}` are not stripped or summarized. They are part of the Codex answer body, so Telegram receives the same final answer that appears in the Codex app.

## Project Registry

Telegram-created threads use the bridge's local project registry so every remote thread starts with an explicit `cwd`.

Manage the registry locally:

```bash
codex-telegram-bridge projects list
codex-telegram-bridge projects add /absolute/path --id bridge --label "Codex Telegram Bridge"
codex-telegram-bridge projects import --limit 25
codex-telegram-bridge projects remove bridge
```

`projects import` bootstraps the registry from observed `cwd` values already cached in the bridge's local `threads_cache` table. Imported ids are derived from the directory name and made unique automatically.

Each Telegram chat/user also keeps a current project selection in the local bridge state. `/new` uses that selected project unless there is only one configured project.

## Slash Commands

The bridge accepts these standalone Telegram commands from the paired chat and allowed user:

- `/start`: pair during setup, or show help after setup
- `/help`: show command help
- `/away`: start remote Codex mode
- `/back`: stop remote Codex mode and clear pending outbound notifications
- `/repair`: restart the shared Codex backend and keep remote mode on
- `/status`: show away mode, pending delivery count, and waiting thread count
- `/project`: list configured projects and recent observed workspaces
- `/project <id>`: switch the current project for future Telegram-created threads
- `/new <prompt>`: create a new Codex thread in the current project and send the prompt immediately
- `/new <project>: <prompt>`: override the current project once for that thread
- `/new`: ask for a prompt; use Telegram's Reply action on the prompt message to create the thread in the selected project

Commands only run as standalone messages. If a message is a Telegram reply to a Codex notification, the text is sent back to that Codex thread verbatim, even if it starts with `/`.

When `/new` succeeds, the bridge sends the selected project `cwd` into `thread/start` and `turn/start`, then includes the confirmed working directory in the Telegram confirmation message.

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

## Token Rotation

If the Telegram bot token is exposed or you want to rotate it:

1. revoke or rotate the token with `@BotFather`
2. run `codex-telegram-bridge telegram disable` if you want to remove the old local config immediately
3. run `codex-telegram-bridge setup --bot-token <new-token>` or `codex-telegram-bridge telegram setup --bot-token <new-token>`
4. restart the daemon if you rotated the token without rerunning the full `setup` flow

The bridge does not keep historical token versions. Once the config file is replaced, only the latest token remains on disk.

## Local Retention And Deletion

The local bridge state lives under `~/.codex-telegram-bridge/` and typically contains:

- `config.json`: bridge/Telegram config and project registry
- `state.db`: cached thread metadata, message routing ids, inbound update dedupe, and other local bridge state
- `daemon.out.log` and `daemon.err.log`: daemon logs

Retention is local and indefinite until you delete or replace those files.

To remove only the Telegram config:

```bash
codex-telegram-bridge telegram disable
```

To remove all local bridge state, stop the daemon first and then delete `~/.codex-telegram-bridge/`.
