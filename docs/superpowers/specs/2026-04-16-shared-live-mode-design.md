# Shared Live Mode Design

Date: 2026-04-16
Repo: `/Users/hanifcarroll/projects/codex-telegram-bridge`

## Summary

Replace the current private spawned app-server model with one supported product mode: a single shared local Codex `app-server` that the bridge uses for all daemon sync, Telegram replies, approvals, and Telegram-created threads.

The user goal is narrow and clear:

- one computer
- away from that computer
- interact with Codex through Telegram
- one Telegram command should make the system live and trustworthy

The current spawned-stdio behavior fails that goal because Telegram replies land in private throwaway backends instead of the canonical live backend. That mode should no longer be part of the normal product surface.

## Product Decision

The bridge will support one intended runtime model:

- shared live backend only

The bridge will no longer present private spawned app-server mode as a user-facing behavior. If a temporary local fallback is kept during implementation, it is an internal migration aid only and should not appear in setup docs, happy-path status text, or product messaging.

## User Experience

### `/live_on`

`/live_on` is the primary Telegram command.

It must:

1. ensure a shared local `codex app-server` is running on loopback
2. point bridge traffic at that shared backend
3. verify the backend is reachable and initialized correctly
4. turn `away on`
5. reply in Telegram with a compact success or failure report

If the shared backend is already healthy, `/live_on` reuses it.

If the backend is missing or unhealthy, `/live_on` recreates it.

### `/live_reset`

`/live_reset` is the explicit recovery path.

It must:

1. stop the managed shared app-server if it exists
2. clear stale runtime state owned by the bridge for that managed backend
3. start a fresh shared app-server
4. verify health
5. keep `away on`
6. reply in Telegram with the new live status

### Existing commands

- `/away_on` and `/away_off` remain presence controls
- `/status` and `/settings` should expose whether shared live mode is configured and healthy
- `/new_thread`, reply routing, and approval buttons continue to work, but always target the shared backend

## Why No Managed CLI Frontend In v1

The user only cares about away-from-keyboard Telegram interaction on one machine.

A detached `codex --remote ...` TUI process does not improve that experience in `v1`. It adds supervision complexity without improving Telegram control. The canonical live runtime can simply be the shared `app-server` process.

Optional local CLI attach can be added later, but it is not required for the bridge to deliver the core product promise.

## Architecture

### Managed shared backend

The bridge manages one local Codex app-server process:

- listen address: loopback only
- initial target: `ws://127.0.0.1:<port>`
- owned by the bridge runtime, not by Codex desktop

The bridge must track:

- configured websocket URL
- managed process PID, if bridge-owned
- last successful health check
- last startup error, if any

This state can live in config plus lightweight runtime metadata under the existing bridge state directory.

### App-server transport abstraction

`CodexAppServerClient` currently assumes spawned stdio. That needs to become a transport-agnostic client with two implementations during migration:

- websocket client for the shared live backend
- temporary spawned stdio client only if needed to land the refactor safely

The end-state product path uses websocket only.

The public bridge code should continue to call a single client API for:

- `initialize`
- `thread/resume`
- `turn/start`
- sync/list/show flows
- notification draining

### Process management

The bridge needs a small manager for the shared backend:

- `ensure_live_backend()`
- `reset_live_backend()`
- `live_backend_status()`

Health should be determined by successful websocket connection and successful JSON-RPC initialization, not just by checking whether a PID exists.

If the Codex websocket listener exposes `/readyz` or `/healthz`, those endpoints can be used as a secondary signal, not the primary source of truth.

## Telegram command behavior

### `/live_on` success response

The Telegram success message should say:

- live mode is on
- whether the backend was reused or restarted
- the websocket URL
- whether `away` is now on

Example shape:

```text
Live mode is on.
Backend: reused
URL: ws://127.0.0.1:4500
Away mode: on
```

### `/live_on` failure response

The Telegram failure message should say:

- which step failed: start, connect, initialize, or away enable
- the immediate next recovery action, usually `/live_reset`

Example shape:

```text
Live mode failed during backend initialization.
The shared Codex backend did not complete the JSON-RPC handshake.
Try /live_reset.
```

## Config changes

`DaemonConfig` needs explicit live-backend configuration.

Proposed fields:

- `codex.liveMode = "shared"`
- `codex.websocketUrl = "ws://127.0.0.1:4500"`
- optional bridge-owned process metadata kept outside the durable config if it is runtime-only

Setup should fail if Telegram is configured but no shared live backend configuration exists.

`doctor` should report:

- configured websocket URL
- whether the shared backend is reachable now
- whether bridge traffic is using shared mode
- clear next step if not healthy

## Migration

### Product migration

User-facing docs and commands should move from:

- "reply routing works whenever daemon is running"

to:

- "reply routing requires live shared mode"

### Runtime migration

Recommended sequence:

1. add websocket transport support
2. add managed live-backend lifecycle helpers
3. add `/live_on` and `/live_reset`
4. switch daemon sync and Telegram reply/approval/new-thread flows to shared mode
5. update `setup`, `doctor`, `/status`, `/settings`, README, and Telegram docs
6. remove user-facing references to private spawned mode

## Error handling

The bridge must distinguish:

- backend not configured
- backend process failed to start
- websocket refused
- JSON-RPC initialize failed
- backend healthy but requested thread action failed

These should produce structured results in CLI JSON and readable Telegram messages.

## Testing

### Unit tests

Add tests for:

- config parsing and redaction for shared live mode
- `/live_on` command parsing
- `/live_reset` command parsing
- backend status classification
- daemon/Telegram flows preferring shared mode

### Integration-style tests

Use the existing fake-codex patterns to cover:

- websocket initialization success
- websocket initialization failure
- `/live_on` reuses healthy backend
- `/live_on` recreates unhealthy backend
- reply routing goes through shared backend metadata instead of private spawn metadata

### Regression expectations

The following must no longer be true in the happy path:

- Telegram replies spawn a private one-off app-server
- daemon sync reads from a different backend than Telegram reply actions

## Non-goals

This design does not attempt to:

- attach the Codex desktop app to the shared backend
- support multiple interactive clients on the same backend
- expose the shared backend beyond the local machine
- solve sleep/wake reliability beyond surfacing clear health state and recovery commands

## Recommendation

Ship shared live backend mode as the only intended product behavior.

For this user, the canonical live session is the shared local Codex app-server itself. Telegram commands control that backend directly. That is the smallest design that matches the real product promise and avoids the trust-breaking behavior of private spawned app-server sessions.
