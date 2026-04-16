# Shared Live Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace private spawned app-server sessions with one managed shared websocket backend, add `/live_on` and `/live_reset`, and route daemon plus Telegram thread actions exclusively through that shared live backend.

**Architecture:** Add explicit shared-backend configuration and a small live-backend manager that owns a loopback `codex app-server --listen ws://127.0.0.1:<port>` process. Refactor `CodexAppServerClient` behind a transport boundary so daemon sync, Telegram replies, approvals, and Telegram-created threads all use the same websocket-connected backend. Then update setup, doctor, status surfaces, and docs so the user-facing product only describes shared live mode.

**Tech Stack:** Rust, clap, rusqlite, serde/serde_json, local process supervision via `std::process`, synchronous websocket client library for JSON-RPC over text frames

---

## File Structure

### New files

- Create: `docs/superpowers/plans/2026-04-16-shared-live-mode.md`
- Create: `src/live.rs`
- Create: `src/ws.rs`

### Modified files

- Modify: `Cargo.toml`
- Modify: `src/cli.rs`
- Modify: `src/codex.rs`
- Modify: `src/config.rs`
- Modify: `src/daemon.rs`
- Modify: `src/lib.rs`
- Modify: `src/state.rs`
- Modify: `src/telegram.rs`
- Modify: `README.md`
- Modify: `docs/telegram.md`

### Responsibilities

- `src/config.rs`: durable config schema for shared live mode
- `src/live.rs`: start, reuse, reset, and inspect the managed local app-server
- `src/ws.rs`: websocket JSON-RPC transport for app-server
- `src/codex.rs`: transport-agnostic client API, request/notification handling
- `src/telegram.rs`: `/live_on`, `/live_reset`, and shared-backend routing for Telegram actions
- `src/daemon.rs`: daemon sync path through shared live backend only
- `src/cli.rs`: CLI surface and parsing tests
- `src/state.rs`: runtime metadata and any lightweight state needed for managed backend status
- `README.md` and `docs/telegram.md`: user-facing cutover from private sessions to shared live mode

## Task 1: Add Shared Live Config And CLI Surface

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/config.rs`
- Modify: `src/cli.rs`
- Modify: `src/lib.rs`
- Test: `src/config.rs`
- Test: `src/cli.rs`

- [ ] **Step 1: Write the failing config and CLI tests**

```rust
#[test]
fn merged_daemon_config_preserves_shared_live_backend() {
    let merged = merged_daemon_config(
        Some(&DaemonConfig {
            version: 4,
            bridge_command: "codex-telegram-bridge".to_string(),
            events: crate::DEFAULT_NOTIFICATION_EVENTS.to_string(),
            telegram: Some(TelegramConfig {
                bot_token: "123:secret".to_string(),
                chat_id: "456".to_string(),
                allowed_user_id: Some("789".to_string()),
            }),
            codex: Some(CodexConfig {
                live_mode: CodexLiveMode::Shared,
                websocket_url: "ws://127.0.0.1:4500".to_string(),
            }),
            projects: vec![],
        }),
        "codex-telegram-bridge",
        crate::DEFAULT_NOTIFICATION_EVENTS,
        TelegramConfig {
            bot_token: "987:secret".to_string(),
            chat_id: "654".to_string(),
            allowed_user_id: Some("321".to_string()),
        },
        CodexConfig {
            live_mode: CodexLiveMode::Shared,
            websocket_url: "ws://127.0.0.1:4500".to_string(),
        },
    );

    assert_eq!(
        merged.codex.as_ref().unwrap().websocket_url,
        "ws://127.0.0.1:4500"
    );
}

#[test]
fn cli_accepts_live_backend_commands() {
    let parsed = Cli::try_parse_from(vec![
        "codex-telegram-bridge",
        "telegram",
        "status",
    ]);
    assert!(parsed.is_ok());

    let parsed = Cli::try_parse_from(vec![
        "codex-telegram-bridge",
        "doctor",
    ]);
    assert!(parsed.is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test merged_daemon_config_preserves_shared_live_backend cli_accepts_live_backend_commands`

Expected: FAIL because `DaemonConfig` lacks explicit Codex shared-backend config and CLI/docs do not yet represent live mode.

- [ ] **Step 3: Add minimal shared-backend config types**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodexConfig {
    pub(crate) live_mode: CodexLiveMode,
    pub(crate) websocket_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CodexLiveMode {
    Shared,
}
```

Add `codex: Option<CodexConfig>` to `DaemonConfig`, require it in `load_daemon_config()`, include it in `merged_daemon_config()`, and redact it in `redacted_daemon_config()`.

- [ ] **Step 4: Add CLI surface for live-mode status and reset semantics**

Extend the Telegram command parser expectations and CLI help text so the product surface can include `/live_on` and `/live_reset`.

```rust
enum TelegramInboundCommand {
    // existing commands...
    LiveOn,
    LiveReset,
}
```

- [ ] **Step 5: Run focused tests to verify they pass**

Run: `cargo test merged_daemon_config_preserves_shared_live_backend telegram_command_parser_supports_core_commands`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/config.rs src/cli.rs src/lib.rs
git commit -m "feat: add shared live backend config surface"
```

## Task 2: Add Websocket App-Server Transport

**Files:**
- Modify: `Cargo.toml`
- Create: `src/ws.rs`
- Modify: `src/codex.rs`
- Modify: `src/lib.rs`
- Test: `src/codex.rs`
- Test: `src/ws.rs`

- [ ] **Step 1: Write the failing websocket transport tests**

```rust
#[test]
fn websocket_app_server_client_initializes_and_sends_notifications() {
    let server = FakeWsAppServer::spawn(vec![
        json!({"id": 1, "result": {"serverInfo": {"userAgent": "test"}}}),
    ]);

    let mut client = CodexAppServerClient::connect_with_backend(
        CodexBackend::SharedWebsocket {
            url: server.url().to_string(),
        },
    )
    .expect("connect websocket backend");

    let response = client
        .request("thread/list", json!({ "limit": 1 }))
        .expect("thread/list");

    assert_eq!(response["threads"].as_array().unwrap().len(), 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test websocket_app_server_client_initializes_and_sends_notifications`

Expected: FAIL because there is no websocket transport or backend selector.

- [ ] **Step 3: Add the websocket dependency and transport module**

Use a synchronous websocket library that supports text frames cleanly. Keep it simple and blocking.

```toml
[dependencies]
tungstenite = { version = "0.24", default-features = false, features = ["handshake"] }
url = "2"
```

Add a minimal transport wrapper:

```rust
pub(crate) struct WsJsonRpcTransport {
    socket: tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
}

impl WsJsonRpcTransport {
    pub(crate) fn connect(url: &str) -> Result<Self> { /* ... */ }
    pub(crate) fn write_json(&mut self, value: &Value) -> Result<()> { /* ... */ }
    pub(crate) fn read_json(&mut self) -> Result<Value> { /* ... */ }
}
```

- [ ] **Step 4: Make `CodexAppServerClient` transport-agnostic**

Replace the hard-coded child/stdin/stdout-only implementation with an internal backend enum.

```rust
enum CodexTransport {
    SharedWebsocket(WsJsonRpcTransport),
}

pub(crate) enum CodexBackend {
    SharedWebsocket { url: String },
}
```

Keep the public request/notify/drain API stable so daemon and Telegram callers do not need per-transport branching.

- [ ] **Step 5: Preserve transport metadata for logs**

The client must still expose:

```rust
pub(crate) fn transport_info(&self) -> CodexAppServerTransportInfo {
    CodexAppServerTransportInfo {
        transport: "shared_websocket",
        app_server_pid: None,
    }
}
```

Change `app_server_pid` to `Option<u32>` because websocket mode is not a child-owned transport.

- [ ] **Step 6: Run focused tests to verify they pass**

Run: `cargo test websocket_app_server_client_initializes_and_sends_notifications app_server_client_collects_notifications_between_requests`

Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/ws.rs src/codex.rs src/lib.rs
git commit -m "feat: add websocket codex app-server transport"
```

## Task 3: Add Managed Live Backend Lifecycle

**Files:**
- Create: `src/live.rs`
- Modify: `src/config.rs`
- Modify: `src/state.rs`
- Modify: `src/lib.rs`
- Test: `src/live.rs`
- Test: `src/state.rs`

- [ ] **Step 1: Write the failing live-backend lifecycle tests**

```rust
#[test]
fn ensure_live_backend_reuses_healthy_backend() {
    let runtime = LiveBackendRuntime::from_status_file(tempdir_path());
    runtime.write_status(LiveBackendStatus {
        websocket_url: "ws://127.0.0.1:4500".to_string(),
        pid: Some(4242),
        healthy: true,
        last_error: None,
    });

    let result = ensure_live_backend(&runtime, &CodexConfig {
        live_mode: CodexLiveMode::Shared,
        websocket_url: "ws://127.0.0.1:4500".to_string(),
    })
    .expect("ensure");

    assert_eq!(result.action, "reused");
}

#[test]
fn reset_live_backend_restarts_unhealthy_backend() {
    let runtime = LiveBackendRuntime::from_status_file(tempdir_path());
    let result = reset_live_backend(&runtime, &shared_codex_config())
        .expect("reset");
    assert_eq!(result.action, "restarted");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ensure_live_backend_reuses_healthy_backend reset_live_backend_restarts_unhealthy_backend`

Expected: FAIL because no managed live-backend module exists.

- [ ] **Step 3: Add runtime metadata helpers**

Track lightweight runtime state in the bridge state directory, for example:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LiveBackendStatus {
    pub(crate) websocket_url: String,
    pub(crate) pid: Option<u32>,
    pub(crate) healthy: bool,
    pub(crate) last_error: Option<String>,
}
```

Persist this in a JSON file under `~/.codex-telegram-bridge/live-backend.json` or an equivalent small runtime artifact.

- [ ] **Step 4: Implement ensure/reset/status helpers**

`src/live.rs` should own:

```rust
pub(crate) fn live_backend_status(config: &CodexConfig) -> Result<LiveBackendStatus> { /* ... */ }
pub(crate) fn ensure_live_backend(config: &CodexConfig) -> Result<EnsureLiveBackendResult> { /* ... */ }
pub(crate) fn reset_live_backend(config: &CodexConfig) -> Result<EnsureLiveBackendResult> { /* ... */ }
```

Use health by real websocket connect + JSON-RPC initialize, not by PID alone.

To start the process:

```rust
Command::new(&resolved.path)
    .arg("app-server")
    .arg("--listen")
    .arg(&config.websocket_url)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()?;
```

- [ ] **Step 5: Run focused tests to verify they pass**

Run: `cargo test ensure_live_backend_reuses_healthy_backend reset_live_backend_restarts_unhealthy_backend`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/live.rs src/config.rs src/state.rs src/lib.rs
git commit -m "feat: add managed shared live backend lifecycle"
```

## Task 4: Route Daemon And Telegram Actions Through Shared Mode

**Files:**
- Modify: `src/daemon.rs`
- Modify: `src/telegram.rs`
- Modify: `src/codex.rs`
- Test: `src/telegram.rs`
- Test: `src/daemon.rs`

- [ ] **Step 1: Write the failing routing tests**

```rust
#[test]
fn telegram_reply_uses_shared_backend_metadata() {
    let result = send_codex_reply_to_thread(
        &conn,
        &shared_codex_config(),
        "thr_1",
        "continue",
        1000,
    )
    .expect("reply");

    assert_eq!(result.pointer("/codex/transport").and_then(Value::as_str), Some("shared_websocket"));
}

#[test]
fn daemon_cycle_reports_backend_error_when_shared_backend_is_unreachable() {
    let result = daemon_cycle(&conn, &config, 1000, Duration::from_secs(1)).expect("cycle");
    assert_eq!(result["action"], "daemon_cycle");
    assert!(result["observed"].as_u64().unwrap() >= 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test telegram_reply_uses_shared_backend_metadata daemon_cycle_reports_backend_error_when_shared_backend_is_unreachable`

Expected: FAIL because daemon and Telegram paths still use `CodexAppServerClient::connect()` with private transport assumptions.

- [ ] **Step 3: Thread config-backed backend selection into daemon and Telegram flows**

Change call sites like:

```rust
let mut client = CodexAppServerClient::connect()?;
```

to:

```rust
let backend = codex_backend_from_config(config)?;
let mut client = CodexAppServerClient::connect_with_backend(backend)?;
```

Update:

- daemon sync
- Telegram reply
- Telegram approval
- Telegram new thread
- MCP paths that should inspect the same live backend

- [ ] **Step 4: Remove happy-path private spawn semantics from Telegram status payloads**

Shared mode should emit transport metadata like:

```json
{
  "codex": {
    "transport": "shared_websocket",
    "websocketUrl": "ws://127.0.0.1:4500"
  }
}
```

Keep failure payloads explicit when the backend cannot be reached.

- [ ] **Step 5: Run focused tests to verify they pass**

Run: `cargo test telegram_reply_uses_shared_backend_metadata daemon_notification_policy_only_enqueues_events_while_away`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/daemon.rs src/telegram.rs src/codex.rs
git commit -m "feat: route daemon and telegram actions through shared backend"
```

## Task 5: Add `/live_on` And `/live_reset`

**Files:**
- Modify: `src/telegram.rs`
- Modify: `src/cli.rs`
- Modify: `src/lib.rs`
- Test: `src/telegram.rs`
- Test: `src/cli.rs`

- [ ] **Step 1: Write the failing command tests**

```rust
#[test]
fn telegram_command_parser_supports_live_commands() {
    assert_eq!(
        parse_telegram_command_text("/live_on"),
        Some(TelegramInboundCommand::LiveOn)
    );
    assert_eq!(
        parse_telegram_command_text("/live_reset"),
        Some(TelegramInboundCommand::LiveReset)
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test telegram_command_parser_supports_live_commands`

Expected: FAIL because the parser and command execution path do not include these commands.

- [ ] **Step 3: Implement `/live_on`**

Add a command execution branch:

```rust
TelegramInboundCommand::LiveOn => {
    let codex = load_daemon_config()?.codex.context("shared backend not configured")?;
    let ensured = ensure_live_backend(&codex)?;
    let away = set_away_mode(conn, true, now)?;
    let sent = telegram_send_text(
        telegram,
        &telegram_live_on_text(&ensured, &away),
        timeout,
    )?;
    Ok(json!({
        "ok": true,
        "action": "telegram_live_on",
        "backend": ensured,
        "away": away,
        "sent": sent
    }))
}
```

- [ ] **Step 4: Implement `/live_reset`**

```rust
TelegramInboundCommand::LiveReset => {
    let codex = load_daemon_config()?.codex.context("shared backend not configured")?;
    let reset = reset_live_backend(&codex)?;
    let away = set_away_mode(conn, true, now)?;
    let sent = telegram_send_text(
        telegram,
        &telegram_live_reset_text(&reset, &away),
        timeout,
    )?;
    Ok(json!({
        "ok": true,
        "action": "telegram_live_reset",
        "backend": reset,
        "away": away,
        "sent": sent
    }))
}
```

- [ ] **Step 5: Run focused tests to verify they pass**

Run: `cargo test telegram_command_parser_supports_live_commands telegram_command_parser_supports_core_commands`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/telegram.rs src/cli.rs src/lib.rs
git commit -m "feat: add telegram live mode commands"
```

## Task 6: Update Doctor, Status, Settings, And Setup Messaging

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/telegram.rs`
- Modify: `README.md`
- Modify: `docs/telegram.md`
- Test: `src/cli.rs`

- [ ] **Step 1: Write the failing surface tests**

```rust
#[test]
fn doctor_reports_shared_live_backend_status() {
    let rendered = render_doctor_result(&doctor_fixture_with_shared_backend());
    assert!(rendered.contains("shared"));
    assert!(rendered.contains("ws://127.0.0.1:4500"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test doctor_reports_shared_live_backend_status`

Expected: FAIL because doctor and settings output do not mention shared live mode.

- [ ] **Step 3: Update doctor and Telegram settings/status text**

Show:

- configured websocket URL
- backend healthy/unhealthy
- away on/off
- `/live_reset` as the primary recovery action when unhealthy

- [ ] **Step 4: Rewrite user docs to remove private-mode happy path**

Update docs to describe:

- setup now includes shared live backend config
- `/live_on` is the normal pre-departure command
- replies require shared live mode

Do not leave any documentation that implies private one-off app-server sessions are acceptable product behavior.

- [ ] **Step 5: Run focused tests to verify they pass**

Run: `cargo test cli_help_smoke`

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/telegram.rs README.md docs/telegram.md src/cli.rs
git commit -m "docs: document shared live mode product flow"
```

## Task 7: Remove Product Reliance On Private Spawned Mode

**Files:**
- Modify: `src/codex.rs`
- Modify: `src/daemon.rs`
- Modify: `src/telegram.rs`
- Test: `src/codex.rs`
- Test: `src/telegram.rs`

- [ ] **Step 1: Write the failing regression tests**

```rust
#[test]
fn connect_without_shared_backend_config_is_not_a_supported_happy_path() {
    let error = codex_backend_from_config(&DaemonConfig {
        codex: None,
        // rest omitted
    })
    .unwrap_err();

    assert!(format!("{error:#}").contains("shared backend"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test connect_without_shared_backend_config_is_not_a_supported_happy_path`

Expected: FAIL because code still treats spawned stdio as the universal default.

- [ ] **Step 3: Remove default private spawn path from the product runtime**

End-state code should reject missing shared-backend config in product paths instead of silently spawning private sessions.

Keep any temporary spawn-only helper gated to tests or clearly internal migration code, not normal runtime code.

- [ ] **Step 4: Run full verification**

Run: `cargo fmt --all --check && cargo test`

Expected: PASS with no failing tests

- [ ] **Step 5: Commit**

```bash
git add src/codex.rs src/daemon.rs src/telegram.rs
git commit -m "refactor: require shared live backend for product runtime"
```

## Spec Coverage Check

- Shared live backend only: Task 1, Task 2, Task 7
- `/live_on` and `/live_reset`: Task 5
- Shared routing for daemon and Telegram actions: Task 4
- Doctor, status, settings, and docs cutover: Task 6
- Managed local backend lifecycle: Task 3
- No detached CLI requirement in v1: preserved by architecture and docs, no implementation task needed beyond avoiding that subsystem

## Placeholder Scan

No `TODO`, `TBD`, or deferred-code placeholders should remain in implementation tasks. If a websocket library API differs slightly in practice, adapt the exact syntax but preserve the task boundary and expected behavior.

## Type Consistency Check

- Durable config type: `CodexConfig`
- runtime mode enum: `CodexLiveMode::Shared`
- lifecycle helpers: `ensure_live_backend`, `reset_live_backend`, `live_backend_status`
- Telegram commands: `TelegramInboundCommand::LiveOn`, `TelegramInboundCommand::LiveReset`
- transport label: `shared_websocket`

