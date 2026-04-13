# codex-hermes-bridge

Rust implementation of `codex-hermes-bridge`: a CLI bridge for inspecting Codex threads, relaying actions, and streaming JSON events for automation.

## Current model

Stable product path:
- thread inspection and actions (`threads`, `show`, `reply`, `approve`, `waiting`, `sync`)
- event streaming via `follow` and `watch`
- hook integration via `watch --exec`
- away-mode notification summaries via `notify-away`

Experimental path:
- `follow --experimental-realtime`
- negotiates `experimentalApi: true`
- attempts dedicated `thread/realtime/*` methods
- should be treated as research/early-adopter functionality, not the backbone

## Install

Build locally:

```bash
cargo build
```

Install the Rust binary:

```bash
cargo install --path .
```

Run through the wrapper without installing:

```bash
bin/codex-hermes-bridge --help
```

The wrapper builds `target/debug/codex-hermes-bridge` on first use and then execs it.

## Commands

### Follow a thread

Stream JSON events for a thread.

```bash
codex-hermes-bridge follow <threadId>
codex-hermes-bridge follow <threadId> --message "please continue" --duration 3000
codex-hermes-bridge follow <threadId> --poll-interval 500
codex-hermes-bridge follow <threadId> --events follow_snapshot,item_completed
codex-hermes-bridge follow <threadId> --experimental-realtime
```

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

Useful flags:
- `--message <text>`: starts a turn before following
- `--duration <ms>`: follow window length
- `--poll-interval <ms>`: periodic thread snapshot refresh cadence
- `--events <csv>`: only emit matching event types
- `--experimental-realtime`: opt into experimental realtime probing

### Watch for changes

Stream JSON events from sync reconciliation plus normalized live notifications.

```bash
codex-hermes-bridge watch
codex-hermes-bridge watch --once
codex-hermes-bridge watch --events thread_waiting,item_completed
codex-hermes-bridge watch --exec "python3 examples/hermes-codex-hook.py"
```

Useful flags:
- `--once`: run one sync pass and return JSON
- `--events <csv>`: filter emitted events
- `--exec <command>`: pipe each emitted event to a hook command on stdin

### Away mode

Track when the user is away and emit attention-needed summaries.

```bash
codex-hermes-bridge away on
codex-hermes-bridge notify-away
codex-hermes-bridge away off
```

`notify-away` syncs live Codex state first, dedupes notifications, and returns thread context for reply or approval prompts.

## Event schema

The stream is newline-delimited JSON. Event types currently include:

Stable watch/follow events:
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

Experimental realtime events:
- `follow_realtime_started`
- `follow_realtime_error`
- `follow_realtime_stopped`
- `follow_realtime_stop_error`
- `follow_realtime_unavailable`

### Common stable event examples

`thread_waiting`

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

`thread_status_changed`

```json
{
  "type": "thread_status_changed",
  "threadId": "thr_123",
  "status": { "type": "active", "activeFlags": [] },
  "raw": { "method": "thread/status/changed", "params": { "threadId": "thr_123" } },
  "thread": {
    "threadId": "thr_123",
    "name": "Need reply"
  }
}
```

`follow_snapshot`

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

### Filtering

Use CSV event names.

```bash
--events thread_waiting,item_completed
--events follow_snapshot,follow_realtime_error
```

### Realtime readiness doctor

Probe whether a thread can be read/followed/realtime-started in the current app-server context.

```bash
codex-hermes-bridge doctor realtime <threadId>
codex-hermes-bridge doctor realtime <threadId> --experimental-realtime
```

This reports per-step readiness for:
- `read_with_turns`
- `read_without_turns`
- `realtime_start`
- `realtime_stop`

Known classified backend failures include:
- `thread_not_loaded`
- `thread_not_materialized`
- `thread_no_rollout`
- `experimental_api_required`

## Experimental API

The bridge can negotiate the Codex app-server capability:

```json
{ "experimentalApi": true }
```

This appears to unlock access to dedicated realtime methods like:
- `thread/realtime/start`
- `thread/realtime/stop`

Important:
- this is not treated as stable product infrastructure
- successful capability negotiation does not guarantee the target thread is usable in realtime mode
- current bridge behavior exposes the result clearly rather than hiding failures

## Product recommendation

Use stable `watch` plus stable `follow` as the main app surface. Use experimental realtime only for probing, research, and early-adopter workflows.
