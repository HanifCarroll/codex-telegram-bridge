# codex-telegram-bridge

`codex-telegram-bridge` lets a local assistant inspect and control Codex threads, and lets you keep working through Telegram when you explicitly mark yourself away.

The product rule is simple:

- when you are present at your computer, Codex does not send Telegram notifications
- when you run `away on`, Codex updates go to Telegram
- replying directly to a bridge-sent Telegram message sends that reply back to the originating Codex thread
- `away off` stops outbound Telegram notifications again

Hermes is optional. It uses the local MCP server when you ask an agent to inspect, reply to, or approve Codex work. Hermes and MCP do not own Telegram notification delivery.

You do not need Hermes for the default product flow. A normal install is Codex plus Telegram plus the local daemon. Add Hermes only if you also want a Hermes agent to call the bridge tools directly.

## Core Product Surface

- Product setup: `setup`
- Presence gate: `away on`, `away off`, `away status`
- Direct Telegram transport: `telegram setup/status/test/disable`
- Telegram remote controls: `/away_on`, `/away_off`, `/status`, `/project`, `/projects`, `/new_thread`, `/inbox`, `/waiting`, `/recent`, `/settings`
- Proactive daemon: `daemon run/install/start/stop/status/logs`
- Project registry: `projects list/add/import/remove`
- Thread inspection: `threads`, `show`, `waiting`, `inbox`
- Thread actions: `reply`, `approve`

Advanced commands such as `sync`, `follow`, `watch`, `new`, `fork`, `archive`, `unarchive`, and `watch --exec` remain available for local automation and maintenance, but they are hidden from default help and are not the recommended OSS onboarding path.

## Optional Agent Adapter

MCP is an optional local adapter for Hermes and other trusted agent clients. It exposes only `doctor`, `threads`, `inbox`, `waiting`, `show`, `reply`, and `approve` through `codex-telegram-bridge mcp`.

MCP does not send proactive notifications, install the daemon, configure Telegram, read Telegram updates, or expose the advanced event stream. Use `setup`, `away`, and the daemon for the Telegram product flow.

## Install

Build locally:

```bash
cargo build
```

Install the binary:

```bash
cargo install --path .
```

Install from Git:

```bash
cargo install --git https://github.com/hanifcarroll/codex-telegram-bridge
```

Download a prebuilt archive from [GitHub Releases](https://github.com/HanifCarroll/codex-telegram-bridge/releases) when a tagged release is available.

Run through the wrapper without installing:

```bash
bin/codex-telegram-bridge --help
```

The wrapper prefers `target/release/codex-telegram-bridge`, falls back to `target/debug/codex-telegram-bridge`, and builds the release binary on first use.

If you need a manual recovery template instead of the interactive setup path, copy [examples/config.example.json](examples/config.example.json) to `~/.codex-telegram-bridge/config.json`, replace the placeholder values, and keep the file mode user-only (`chmod 600 ~/.codex-telegram-bridge/config.json`). For token-only setup from environment, see [examples/telegram.env.example](examples/telegram.env.example).

## Quick Start

Inspect your local Codex and bridge setup:

```bash
codex-telegram-bridge doctor
```

Configure Telegram and the daemon in one command:

```bash
codex-telegram-bridge setup --bot-token <telegram-bot-token>
```

No Hermes setup is required for Telegram notifications or Telegram replies.

For non-interactive setup:

```bash
codex-telegram-bridge setup \
  --bot-token <telegram-bot-token> \
  --chat-id <telegram-chat-id> \
  --allowed-user-id <telegram-user-id>
```

Test Telegram delivery:

```bash
codex-telegram-bridge telegram test --message "Codex bridge is ready"
```

Turn on remote notifications when you leave your computer:

```bash
codex-telegram-bridge away on
```

Turn them off when you are back:

```bash
codex-telegram-bridge away off
```

Optional: if you want Hermes to control Codex when you ask it directly:

```bash
codex-telegram-bridge hermes install --dry-run
codex-telegram-bridge hermes install
```

Restart Hermes after registration so it reconnects to MCP servers and discovers the `codex_*` tools.

## How It Works

`setup` writes `~/.codex-telegram-bridge/config.json` with user-only permissions, clears any existing Telegram webhook for the bot token, installs the local daemon service unless disabled, and can optionally register the Hermes MCP server.

The daemon runs locally. Each cycle:

1. syncs Codex thread state
2. checks the local away state
3. enqueues new notification events only when away is on
4. sends queued events to Telegram
5. processes Telegram replies and approval button callbacks

Inbound Telegram replies are processed whenever the daemon is running. The away gate only controls outbound notifications.

Telegram notifications use a compact header, keep Codex's answer body verbatim, and omit internal thread ids. To continue the conversation remotely, use Telegram's Reply action on the specific Codex notification.

Telegram-created threads run in an explicit registered project working directory. Set the current project from Telegram with `/project <id>`, inspect choices with `/projects`, or manage the registry locally with `codex-telegram-bridge projects ...`.

Use a Telegram bot token dedicated to this bridge. Telegram update delivery should have one owner.

## Commands

### Setup And Doctor

```bash
codex-telegram-bridge setup --bot-token <telegram-bot-token>
codex-telegram-bridge setup --bot-token <telegram-bot-token> --register-hermes
codex-telegram-bridge doctor
```

Useful setup flags:

- `--chat-id <id>`: skip `/start` pairing
- `--allowed-user-id <id>`: restrict inbound replies/buttons to one Telegram user
- `--no-install-daemon`: write config without installing a service
- `--no-start-daemon`: install without starting the service
- `--register-hermes`: also run `hermes mcp add`
- `--dry-run`: print the planned shape without changing files or services

### Presence Gate

```bash
codex-telegram-bridge away status
codex-telegram-bridge away on
codex-telegram-bridge away off
```

`away on` starts a new away session. The daemon only sends events observed after that session started, so old waiting threads do not flood Telegram when you leave. `away off` clears pending outbound notifications so delayed retries do not notify you after you return.

### Telegram

```bash
codex-telegram-bridge telegram setup --bot-token <telegram-bot-token>
codex-telegram-bridge telegram setup \
  --bot-token <telegram-bot-token> \
  --chat-id <telegram-chat-id> \
  --allowed-user-id <telegram-user-id>
codex-telegram-bridge telegram test --message "test"
codex-telegram-bridge telegram status
codex-telegram-bridge telegram disable
```

See [docs/telegram.md](docs/telegram.md).

Release mechanics are documented in [docs/releasing.md](docs/releasing.md).

### Projects

```bash
codex-telegram-bridge projects list
codex-telegram-bridge projects add /absolute/path --id bridge --label "Codex Telegram Bridge"
codex-telegram-bridge projects import --limit 25
codex-telegram-bridge projects remove bridge
```

Use the project registry to give Telegram-created threads deterministic working directories. `projects import` suggests entries from observed Codex thread `cwd` values in the local state cache; `projects add` is the explicit source of truth.

### Daemon

```bash
codex-telegram-bridge daemon run --once
codex-telegram-bridge daemon run
codex-telegram-bridge daemon install --dry-run
codex-telegram-bridge daemon install
codex-telegram-bridge daemon start
codex-telegram-bridge daemon stop
codex-telegram-bridge daemon status
codex-telegram-bridge daemon logs
```

`daemon install` writes a user service:

- macOS: `~/Library/LaunchAgents/com.hanifcarroll.codex-telegram-bridge.plist`
- Linux: `~/.config/systemd/user/com.hanifcarroll.codex-telegram-bridge.service`

### Inspect And Act On Threads

```bash
codex-telegram-bridge threads --limit 25
codex-telegram-bridge show <threadId>
codex-telegram-bridge waiting --limit 25
codex-telegram-bridge inbox --limit 25
codex-telegram-bridge reply <threadId> --message "please continue"
codex-telegram-bridge approve <threadId> --decision approve
```

### Follow And Watch

```bash
codex-telegram-bridge follow <threadId>
codex-telegram-bridge follow <threadId> --message "please continue" --duration 3000
codex-telegram-bridge follow <threadId> --events follow_snapshot,item_completed
codex-telegram-bridge watch --once
codex-telegram-bridge watch --events thread_waiting,thread_completed,item_completed
codex-telegram-bridge watch --exec "python3 examples/print-hook-event.py"
```

`watch --exec` is for trusted local automation. It pipes each event to the command on stdin.

### Optional Hermes MCP

```bash
codex-telegram-bridge mcp
codex-telegram-bridge hermes install --dry-run
codex-telegram-bridge hermes install
codex-telegram-bridge hermes install --hermes-command hermes-se
```

The MCP server exposes `doctor`, `threads`, `inbox`, `waiting`, `show`, `reply`, and `approve` tools, plus thread resources and safe prompts.

See [docs/hermes.md](docs/hermes.md).

## Event Schema

The stream is newline-delimited JSON. Common event types include:

- `watch_started`
- `thread_waiting`
- `thread_completed`
- `thread_status_changed`
- `turn_started`
- `item_started`
- `item_completed`
- `notification`
- `follow_started`
- `follow_snapshot`
- `follow_turn_started`
- `thread_error`

Example `thread_waiting` event:

```json
{
  "type": "thread_waiting",
  "threadId": "thr_123",
  "promptKind": "reply",
  "thread": {
    "threadId": "thr_123",
    "name": "Need reply",
    "statusType": "active"
  }
}
```

## Notes

- Hook commands are arbitrary local code. Only run trusted commands with `watch --exec`.
- MCP tools can read and mutate local Codex threads. Only register this server with trusted local agents.
- The Telegram bot token is stored in the local bridge config and redacted from command output.
- `doctor` is the fastest way to verify that the Codex binary and bridge configuration are discoverable from your environment.
