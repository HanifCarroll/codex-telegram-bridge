# codex-hermes-bridge

`codex-hermes-bridge` is a Rust CLI for inspecting Codex threads, replying or approving when Codex needs attention, and streaming normalized JSON events for automation.

## Stable public surface

This repo is currently focused on a small stable surface:
- inspect threads and waiting work (`threads`, `show`, `waiting`, `inbox`, `sync`)
- take action on a thread (`reply`, `approve`)
- stream thread updates (`follow`, `watch`)
- archive or unarchive threads (`archive`, `unarchive`)
- emit away-mode summaries (`away`, `notify-away`)
- expose a local MCP control plane for Hermes (`mcp`, `hermes install`)
- push Codex watch events into Hermes through signed local webhooks (`hermes post-webhook`)

Everything documented below is intended to be part of that public surface.

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
cargo install --git https://github.com/hanifcarroll/codex-hermes-bridge
```

Run through the wrapper without installing:

```bash
bin/codex-hermes-bridge --help
```

The wrapper prefers `target/release/codex-hermes-bridge`, falls back to `target/debug/codex-hermes-bridge`, and builds the release binary on first use.

`codex-hermes-bridge` resolves the Codex binary by checking each candidate with `codex --version`.
If `CODEX_BIN` points at a missing or broken executable, the bridge skips it and falls back to other known candidates before using `PATH`.

## Quick start

Inspect the current Codex setup:

```bash
codex-hermes-bridge doctor
```

List recent threads:

```bash
codex-hermes-bridge threads --limit 5
```

Run a one-shot watch pass and return normalized events:

```bash
codex-hermes-bridge watch --once
```

Register the bridge with Hermes. This installs the MCP control lane. Add `--webhook-deliver` to also install the proactive notification lane:

```bash
codex-hermes-bridge hermes install --dry-run
codex-hermes-bridge hermes install
codex-hermes-bridge hermes install \
  --webhook-deliver telegram \
  --webhook-deliver-chat-id <chat-id>
```

If you use a profile wrapper, pass it explicitly:

```bash
codex-hermes-bridge hermes install --hermes-command hermes-se
```

Restart Hermes after registration so it reconnects to MCP servers and discovers the `codex_*` tools. If you installed the notification lane, run the printed `notificationLane.watcherCommand` as a long-lived local process. That command is what makes Codex changes notify Hermes without Hermes asking first.

## Commands

### Inspect threads

```bash
codex-hermes-bridge threads --limit 25
codex-hermes-bridge show <threadId>
codex-hermes-bridge waiting --limit 25
codex-hermes-bridge sync --limit 50
```

### Reply or approve

```bash
codex-hermes-bridge reply <threadId> --message "please continue"
codex-hermes-bridge approve <threadId> --decision approve
```

### Follow a thread

Stream normalized JSON events for a thread.

```bash
codex-hermes-bridge follow <threadId>
codex-hermes-bridge follow <threadId> --message "please continue" --duration 3000
codex-hermes-bridge follow <threadId> --poll-interval 500
codex-hermes-bridge follow <threadId> --events follow_snapshot,item_completed
```

Useful flags:
- `--message <text>`: starts a turn before following
- `--duration <ms>`: follow window length
- `--poll-interval <ms>`: periodic thread snapshot refresh cadence
- `--events <csv>`: only emit matching event types

### Composed follow flows

Run the action and follow in the same app-server session.

```bash
codex-hermes-bridge new --message "start work" --follow
codex-hermes-bridge reply <threadId> --message "please continue" --follow
codex-hermes-bridge approve <threadId> approve --follow
codex-hermes-bridge fork <threadId> --message "try another path" --follow
```

By default, composed follow events are embedded in the final JSON result at `follow.events`.

For live JSONL streaming instead, add `--stream`:

```bash
codex-hermes-bridge new --message "start work" --follow --stream --events follow_snapshot,item_completed
```

### Watch for changes

Stream normalized JSON events from sync reconciliation.

```bash
codex-hermes-bridge watch
codex-hermes-bridge watch --once
codex-hermes-bridge watch --events thread_waiting,thread_completed,item_completed
codex-hermes-bridge watch --exec "python3 examples/print-hook-event.py"
```

Useful flags:
- `--once`: run one sync pass and return JSON
- `--events <csv>`: filter emitted events
- `--exec <command>`: pipe each emitted event to a hook command on stdin

### Push watch events to Hermes

`hermes post-webhook` reads one watch event from stdin, wraps it for Hermes, signs it with `X-Hub-Signature-256`, and posts it to a local Hermes webhook route.

```bash
codex-hermes-bridge watch \
  --events thread_waiting,thread_completed,thread_status_changed \
  --exec 'codex-hermes-bridge hermes post-webhook --url http://localhost:8644/webhooks/codex-watch --secret <secret>'
```

Use `codex-hermes-bridge hermes install --webhook-deliver ...` to generate the exact command for your Hermes profile.

### Archive threads

Archive explicit threads or selected inbox rows.

```bash
codex-hermes-bridge archive --thread-id thr_123 --dry-run
codex-hermes-bridge archive --project my-app --status idle --dry-run
codex-hermes-bridge archive --project my-app --status idle --yes
codex-hermes-bridge unarchive thr_123 --dry-run
```

Archive commands that select from the inbox are treated as bulk operations.
They require `--dry-run` or `--yes`; explicit thread IDs do not require the bulk confirmation.

### Away mode

Track when you are away and emit attention-needed summaries.

```bash
codex-hermes-bridge away on
codex-hermes-bridge notify-away
codex-hermes-bridge notify-away --no-completed
codex-hermes-bridge away off
```

`notify-away` syncs live Codex state first, dedupes notifications, and returns thread context for reply, approval, and completed-thread prompts. Completed-thread notifications are included by default; pass `--no-completed` to only emit active attention prompts.

### Hermes MCP

Run a stdio MCP server that exposes Codex thread tools to Hermes:

```bash
codex-hermes-bridge mcp
```

The MCP server exposes structured tools for `doctor`, `threads`, `inbox`, `waiting`, `show`, `new`, `fork`, `reply`, `approve`, `archive`, `unarchive`, `away`, and `notify-away`. It also exposes resources for thread context and prompts for safe inbox/reply/approval workflows.

`watch` is intentionally not exposed as an MCP tool. It is a long-running event stream and belongs in the notification lane with `watch --exec`, not in the request/response tool lane.

See [docs/hermes.md](docs/hermes.md) for the Hermes-first setup guide.

## Hook examples

`watch --exec` pipes one event at a time to the given command on stdin. Hook stdout and stderr pass through to the terminal, and a nonzero hook exit fails the bridge command.

### Print matching events

```bash
codex-hermes-bridge watch --exec "python3 examples/print-hook-event.py"
```

### Send Telegram notifications

```bash
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...
codex-hermes-bridge watch --exec "python3 examples/notify-telegram.py"
```

The Telegram example expects:
- `TELEGRAM_BOT_TOKEN`
- `TELEGRAM_CHAT_ID`

Do not hardcode secrets in the example file or commit them to the repo.

## Event schema

The stream is newline-delimited JSON.
Common stable event types include:
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

Example `follow_snapshot` event:

```json
{
  "type": "follow_snapshot",
  "threadId": "thr_123",
  "thread": {
    "id": "thr_123",
    "name": "Need reply"
  }
}
```

## Notes

- Hook commands are arbitrary local code. Only run trusted commands with `watch --exec`.
- MCP tools can read and mutate local Codex threads. Only register this server with trusted local agents.
- The bridge stores local state in its SQLite cache so repeated commands can reconcile thread state efficiently.
- `doctor` is the fastest way to verify that the Codex binary can actually be resolved from your environment.
