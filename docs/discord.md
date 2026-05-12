# Discord Setup

Discord is an alternate bridge-owned remote control surface for Codex.

The intended behavior matches the Telegram flow:

- no Discord notifications while you are at your computer
- `/away` enables outbound Discord notifications
- `/back` disables outbound Discord notifications
- `/threads` sends recent Codex threads as reply-routable Discord messages
- replies to bridge-sent Discord messages are routed back to the originating Codex thread
- slash commands can toggle away mode, inspect state, pick a project, and start new Codex threads from Discord

## Create A New Discord Bot

Create a new application in the Discord Developer Portal. Do not reuse an existing bot token.

Required bot permissions for the target channel:

- View Channel
- Send Messages
- Read Message History
- Send typing indicators through the channel typing endpoint

Enable the Message Content Intent for the bot. The bridge reads normal channel messages so it can process `/away`, `/threads`, and Discord replies.

Invite the bot to each server, then copy the channel id for every channel that should receive Codex updates.

## Setup

```bash
codex-telegram-bridge discord setup \
  --bot-token <discord-bot-token> \
  --channel-id <discord-channel-id>
```

Pass `--channel-id` more than once to send updates to multiple Discord channels:

```sh
codex-telegram-bridge discord setup \
  --bot-token <discord-bot-token> \
  --channel-id <openclaw-agent-channel-id> \
  --channel-id <hermes-agent-channel-id>
```

The token can also come from `DISCORD_BOT_TOKEN`:

```bash
DISCORD_BOT_TOKEN=<discord-bot-token> \
codex-telegram-bridge discord setup --channel-id <discord-channel-id>
```

Restrict inbound commands and replies to one Discord user when needed:

```bash
codex-telegram-bridge discord setup \
  --bot-token <discord-bot-token> \
  --channel-id <discord-channel-id> \
  --allowed-user-id <discord-user-id>
```

Setup writes `~/.codex-telegram-bridge/config.json` with user-only file permissions and sends a confirmation message to the configured Discord channel.

## Test Delivery

```bash
codex-telegram-bridge discord test --message "Codex bridge is ready"
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

## Discord Commands

The bridge accepts these standalone Discord messages from the configured channel and allowed user:

- `/start`: show help
- `/help`: show command help
- `/away`: start remote Codex mode
- `/back`: stop remote Codex mode and clear pending outbound notifications
- `/status`: show away mode, pending delivery count, and waiting thread count
- `/telegram_on`: enable Telegram delivery and bot polling
- `/telegram_off`: disable Telegram delivery and bot polling
- `/discord_on`: enable Discord delivery and bot polling
- `/discord_off`: disable Discord delivery and bot polling
- `/threads`: send the 5 most recent Codex threads as separate reply-routable messages
- `/threads <count>`: send that many recent Codex threads, up to 25
- `/project`: list configured projects and recent observed workspaces
- `/project <id>`: switch the current project for future Discord-created threads
- `/new <prompt>`: create a new Codex thread in the current project and send the prompt immediately
- `/new <project>: <prompt>`: override the current project once for that thread
- `/new`: ask for a prompt; use Discord's Reply action on the prompt message to create the thread in the selected project

Commands only run as standalone messages. If a message is a Discord reply to a Codex notification, the text is sent back to that Codex thread verbatim, even if it starts with `/`.

## Channel Toggles

Discord setup stays in the local config when disabled. Use these commands to pause or resume Discord without losing the bot token or channel ids:

```bash
codex-telegram-bridge discord disable
codex-telegram-bridge discord enable
```

Telegram and Discord can also toggle each other through `/telegram_on`, `/telegram_off`, `/discord_on`, and `/discord_off`.

## Reply Flow

When Codex needs attention and you are away, the daemon sends a Discord message. Reply directly to that Discord message with the exact text you want sent to Codex. The daemon polls recent channel messages, looks up the original Discord message id in SQLite, sends the reply through the shared live Codex backend, and lets the next sync cycle deliver Codex's answer when the turn finishes.

After a Discord reply or `/new` prompt starts a Codex turn, the daemon refreshes Discord's typing indicator for that channel until the answer is delivered or the short-lived typing window expires.

For approval prompts, reply `YES` or `NO` to the approval message.

## Security Notes

- Use a new Discord bot dedicated to this bridge.
- The Discord bot token is stored only in the local daemon config file.
- Command output redacts the bot token.
- `allowed_user_id` restricts inbound Discord replies and commands to one Discord user.
- The bot should only be invited to the server/channel where you want Codex updates.
