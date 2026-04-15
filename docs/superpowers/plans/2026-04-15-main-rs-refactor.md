# `main.rs` Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the current binary-only `src/main.rs` into a thin binary plus focused library modules without changing CLI behavior, Telegram behavior, daemon behavior, MCP behavior, or state schema.

**Architecture:** Convert the crate to `src/lib.rs` plus a minimal `src/main.rs` wrapper first, then extract subsystems in dependency order: CLI, config, project helpers, SQLite state, Codex app-server logic, Telegram, daemon, and MCP. Keep functions and structs private by default inside each module, expose only the small surface needed by sibling modules, and keep unit tests next to the private code they exercise.

**Tech Stack:** Rust 2021, Clap 4, rusqlite, serde/serde_json, ureq, std test harness

---

## Rust Guidance Applied

- Keep the binary entrypoint thin and move important logic into `src/lib.rs` so integration tests can import the crate. Source: [Rust Book, Test Organization](https://doc.rust-lang.org/book/ch11-03-test-organization.html).
- Organize related code into modules that mirror the file tree, and use `src/foo.rs` or `src/foo/bar.rs` paths instead of keeping one giant crate root. Source: [Rust Book, Modules](https://doc.rust-lang.org/book/ch07-02-defining-modules-to-control-scope-and-privacy.html).
- Keep items private by default and make visibility explicit only where cross-module access is required. Source: [Rust Book, Modules](https://doc.rust-lang.org/book/ch07-02-defining-modules-to-control-scope-and-privacy.html).
- Prefer `src/foo.rs` plus nested `src/foo/bar.rs` over leaning on `mod.rs` everywhere; use `mod.rs` only when a nested module directory actually benefits from it. Source: [Rust Book, Separating Modules into Different Files](https://doc.rust-lang.org/book/ch07-05-separating-modules-into-different-files.html).
- Preserve style-guide import ordering and avoid incidental churn while moving code. Source: [Rust Style Guide, Items and Imports](https://doc.rust-lang.org/style-guide/items.html).

## Target File Structure

**Create:**

- `src/lib.rs` - crate root for all application logic, module declarations, `main_entry`, and top-level command dispatch
- `src/cli.rs` - `Cli`, `Commands`, and all Clap subcommand enums plus parse tests
- `src/config.rs` - daemon config structs plus config load/write/merge/redaction helpers
- `src/projects.rs` - project slug/alias/cwd normalization, query resolution, and current-thread project resolution
- `src/state.rs` - SQLite schema, migrations, state persistence, route tables, settings helpers, and DB-backed queries
- `src/codex.rs` - Codex app-server client plus thread/follow/watch/new/fork/archive helpers
- `src/telegram.rs` - Telegram API helpers, rendering, slash-command parsing, route extraction, and prompt/callback reply handling
- `src/daemon.rs` - daemon polling loop plus service install/start/stop/status/log helpers
- `src/mcp.rs` - MCP stdio server, protocol handlers, and tool metadata
- `tests/cli_help.rs` - binary smoke test proving the thin wrapper still works

**Modify:**

- `src/main.rs` - reduce to the binary wrapper that calls into the library and handles process exit
- `README.md` - contributor note describing the new module layout after the refactor lands

**Do not create:**

- `src/types.rs`
- `src/utils.rs`
- `src/helpers.rs`
- `src/prelude.rs`

Those files become junk drawers quickly. Keep types and helpers with the subsystem that owns them.

---

### Task 1: Create the Library Boundary First

**Files:**
- Create: `src/lib.rs`
- Create: `tests/cli_help.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Add a failing binary smoke test**

Create `tests/cli_help.rs`:

```rust
use std::process::Command;

#[test]
fn cli_help_smoke() {
    let output = Command::new(env!("CARGO_BIN_EXE_codex-telegram-bridge"))
        .arg("--help")
        .output()
        .expect("run binary");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("telegram"));
    assert!(stdout.contains("projects"));
}
```

- [ ] **Step 2: Run the smoke test to verify the current binary still passes**

Run: `cargo test --test cli_help`

Expected: PASS

- [ ] **Step 3: Move the application entrypoint into `src/lib.rs`**

Create `src/lib.rs` with the current crate contents from `src/main.rs`, but replace the binary `main` function with library entrypoints:

```rust
pub fn main_entry() -> anyhow::Result<()> {
    run()
}

pub fn render_error_envelope(error: &anyhow::Error) -> String {
    let envelope = ErrorEnvelope {
        ok: false,
        error: ErrorBody {
            code: "internal_error",
            message: format!("{error:#}"),
            classified: classify_app_server_error_message(&format!("{error:#}")),
        },
    };

    serde_json::to_string(&envelope).expect("serialize error envelope")
}
```

Keep the existing `run()` body unchanged in this task. This task is mechanical isolation, not behavioral refactoring.

- [ ] **Step 4: Reduce `src/main.rs` to a thin wrapper**

Replace `src/main.rs` with:

```rust
fn main() {
    if let Err(error) = codex_telegram_bridge::main_entry() {
        println!("{}", codex_telegram_bridge::render_error_envelope(&error));
        std::process::exit(1);
    }
}
```

- [ ] **Step 5: Run the full test suite**

Run: `cargo test`

Expected: PASS

- [ ] **Step 6: Commit**

Run:

```bash
git add src/lib.rs src/main.rs tests/cli_help.rs
git commit -m "Refactor crate into lib plus thin binary"
```

---

### Task 2: Extract the Clap Schema Into `src/cli.rs`

**Files:**
- Create: `src/cli.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Move all Clap types into `src/cli.rs`**

Move these definitions from `src/lib.rs` into `src/cli.rs`:

- `Cli`
- `Commands`
- `AwayCommands`
- `HermesCommands`
- `DaemonCommands`
- `TelegramCommands`
- `ProjectCommands`

At the top of `src/lib.rs`, replace the in-file definitions with:

```rust
mod cli;

use crate::cli::{
    AwayCommands, Cli, Commands, DaemonCommands, HermesCommands, ProjectCommands,
    TelegramCommands,
};
```

- [ ] **Step 2: Move the CLI parser tests with the CLI module**

Move these tests from the bottom of the current crate test module into `src/cli.rs` under `#[cfg(test)]`:

- `cli_accepts_product_setup_command`
- `cli_accepts_daemon_lifecycle_commands`
- `cli_accepts_telegram_setup_and_status_commands`
- `cli_accepts_project_registry_commands`
- `cli_accepts_hermes_install_command`
- `cli_exposes_version_and_command_descriptions`

- [ ] **Step 3: Run targeted CLI tests**

Run: `cargo test cli_accepts_`

Expected: PASS

- [ ] **Step 4: Run the full suite**

Run: `cargo test`

Expected: PASS

- [ ] **Step 5: Commit**

Run:

```bash
git add src/cli.rs src/lib.rs
git commit -m "Extract CLI definitions into module"
```

---

### Task 3: Extract Config and Project Logic

**Files:**
- Create: `src/config.rs`
- Create: `src/projects.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Move daemon config types and config persistence into `src/config.rs`**

Move these items into `src/config.rs`:

- `DaemonConfig`
- `TelegramConfig`
- `RegisteredProject`
- `TelegramSetupOptions`
- `SetupOptions`
- `daemon_config_path`
- `merged_daemon_config`
- `write_daemon_config`
- `load_daemon_config`
- `redacted_daemon_config`
- `read_daemon_config_raw`
- `resolve_telegram_bot_token`

Expose only the functions the rest of the crate needs:

```rust
pub(crate) use config::{
    load_daemon_config, merged_daemon_config, read_daemon_config_raw, redacted_daemon_config,
    resolve_telegram_bot_token, write_daemon_config, DaemonConfig, RegisteredProject,
    SetupOptions, TelegramConfig, TelegramSetupOptions,
};
```

- [ ] **Step 2: Move pure project helpers into `src/projects.rs`**

Move these items into `src/projects.rs`:

- `derive_project_label`
- `slugify_project_token`
- `normalize_project_aliases`
- `ensure_unique_project_id`
- `canonicalize_project_cwd`
- `build_registered_project`
- `resolve_project_query`
- `ResolvedNewThreadRequest`
- `resolve_new_thread_request`

Do **not** move `observed_workspaces_from_db` yet; it depends on SQLite and belongs with state access.

- [ ] **Step 3: Move project/config tests with their owning modules**

Move these tests into `src/config.rs` or `src/projects.rs` as appropriate:

- `merged_daemon_config_preserves_existing_projects`
- `project_query_matches_id_alias_and_label`
- `new_thread_request_uses_current_project_and_override`

- [ ] **Step 4: Run targeted tests**

Run:

```bash
cargo test merged_daemon_config_preserves_existing_projects
cargo test project_
cargo test new_thread_request_uses_current_project_and_override
```

Expected: PASS

- [ ] **Step 5: Commit**

Run:

```bash
git add src/config.rs src/projects.rs src/lib.rs
git commit -m "Extract config and project helpers"
```

---

### Task 4: Extract SQLite State and Route Persistence

**Files:**
- Create: `src/state.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Move all SQLite and filesystem state helpers into `src/state.rs`**

Move these groups into `src/state.rs`:

- SQL integer conversion helpers
- DB creation and migration helpers
- settings getters/setters
- thread snapshot persistence
- action recording
- delivery log and route table helpers
- waiting/inbox queries
- observed workspace query
- Telegram current-project settings

Specifically include:

- `create_state_db`
- `create_state_db_in_memory`
- `init_state_db`
- `table_columns`
- `ensure_column`
- `state_dir_path`
- `state_db_path`
- `get_setting_number`
- `set_setting`
- `set_setting_text`
- `get_setting_text`
- `telegram_current_project_key`
- `get_telegram_current_project_id`
- `set_telegram_current_project_id`
- `observed_workspaces_from_db`
- `insert_telegram_message_route`
- `insert_telegram_callback_route`
- `insert_telegram_command_route`
- `lookup_telegram_message_route`
- `lookup_telegram_command_route`
- `mark_telegram_command_route_used`

- [ ] **Step 2: Keep the module API narrow**

In `src/lib.rs`, import only what the callers use. Do not `pub use state::*;`.

Preferred pattern:

```rust
use crate::state::{
    create_state_db, get_telegram_current_project_id, mark_telegram_command_route_used,
    observed_workspaces_from_db, set_telegram_current_project_id, state_db_path,
};
```

- [ ] **Step 3: Move DB-focused tests into `src/state.rs`**

Move these tests into the state module:

- `state_schema_matches_ts_bridge_contract`
- `create_state_db_migrates_legacy_threads_cache_columns`
- `persists_snapshots_and_actions_in_sqlite_state`
- `project_current_selection_round_trips_per_identity`
- `project_import_suggests_unique_ids_from_observed_workspaces`
- `telegram_routes_map_message_replies_to_codex_threads`
- `telegram_command_prompt_routes_map_reply_to_new_thread_prompt`
- `telegram_callback_routes_map_buttons_to_approvals`
- `telegram_inbound_log_dedupes_processed_updates_per_bot`

- [ ] **Step 4: Run targeted state tests**

Run:

```bash
cargo test state_schema_matches_ts_bridge_contract
cargo test project_current_selection_round_trips_per_identity
cargo test telegram_routes_map_message_replies_to_codex_threads
```

Expected: PASS

- [ ] **Step 5: Run the full suite and commit**

Run:

```bash
cargo test
git add src/state.rs src/lib.rs
git commit -m "Extract sqlite state and route persistence"
```

---

### Task 5: Extract Codex App-Server and Thread Lifecycle Logic

**Files:**
- Create: `src/codex.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Move the Codex app-server client and thread lifecycle helpers into `src/codex.rs`**

Move these groups together:

- `CodexAppServerClient`
- app-server reader types and message handling
- new/fork/archive/unarchive helpers
- follow/watch helpers
- thread snapshot normalization
- attention/inbox classification helpers
- binary resolution helpers

The point of this task is to keep Codex-facing transport and thread-domain logic in one place so Telegram, daemon, MCP, and CLI command handlers all depend on the same subsystem.

- [ ] **Step 2: Avoid a fake abstraction layer**

Do **not** create a trait just to wrap `CodexAppServerClient`. Keep the real type and move functions as free functions inside `src/codex.rs`.

Expose a minimal surface like:

```rust
pub(crate) use codex::{
    archive_from_db, build_show_thread_result, classify_app_server_error_message,
    collect_follow_events, normalize_thread_snapshot, resolve_codex_binary,
    start_codex_watch_receiver, sync_state_from_live, thread_start_params, turn_start_params,
    CodexAppServerClient,
};
```

- [ ] **Step 3: Move Codex-focused tests into `src/codex.rs`**

Move tests covering:

- app-server message parsing
- follow/watch behavior
- archive/new/fork behavior
- binary resolution
- inbox/waiting classification

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test app_server_
cargo test follow_
cargo test watch_
```

Expected: PASS

- [ ] **Step 5: Commit**

Run:

```bash
git add src/codex.rs src/lib.rs
git commit -m "Extract codex app-server and thread lifecycle logic"
```

---

### Task 6: Extract the Telegram Subsystem

**Files:**
- Create: `src/telegram.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Move Telegram transport, rendering, and command handling into `src/telegram.rs`**

Move these groups:

- Telegram HTTP API helpers
- message send helpers
- command registration helpers
- command parsing and authorization
- notification rendering and splitting
- file-reference compaction
- reply-route extraction
- callback-route extraction
- Telegram command execution
- new-thread prompt reply execution

- [ ] **Step 2: Keep the API file-oriented**

Start with a single `src/telegram.rs`. If it still exceeds about 1,500 lines after extraction, immediately split it into:

- `src/telegram.rs` - module root and public surface
- `src/telegram/api.rs` - raw Telegram HTTP helpers
- `src/telegram/render.rs` - message formatting/splitting/sanitizing
- `src/telegram/routes.rs` - reply and callback route lookup/extraction
- `src/telegram/commands.rs` - slash command parsing and execution

Use `src/telegram.rs` plus child files, not a root `src/telegram/mod.rs`, unless the module directory needs a real root file with substantial logic.

- [ ] **Step 3: Move Telegram tests with the Telegram module**

Move tests covering:

- slash command parsing
- authorization
- payload rendering
- notification splitting
- file-reference compaction
- confirmation text

- [ ] **Step 4: Run focused Telegram tests**

Run:

```bash
cargo test telegram_
```

Expected: PASS

- [ ] **Step 5: Commit**

Run:

```bash
git add src/telegram.rs src/telegram src/lib.rs
git commit -m "Extract telegram subsystem"
```

---

### Task 7: Extract Daemon and MCP

**Files:**
- Create: `src/daemon.rs`
- Create: `src/mcp.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Move daemon run-loop and service management into `src/daemon.rs`**

Move:

- `run_daemon`
- daemon install/uninstall/start/stop/status/log helpers
- service specification helpers
- outbound notification enqueue/delivery policy
- Telegram update processing loop

- [ ] **Step 2: Move MCP server code into `src/mcp.rs`**

Move:

- `run_mcp_server`
- protocol message handling
- tool/resource/prompt metadata builders
- MCP argument parsing
- MCP tool execution routing

- [ ] **Step 3: Keep top-level dispatch in `src/lib.rs`, but make it boring**

After this extraction, `src/lib.rs` should mainly do:

- `mod ...` declarations
- imports
- `main_entry`
- `render_error_envelope`
- `run()` with the top-level `match cli.command { ... }`

The `run()` match should call small module functions, not inline hundreds of lines of logic.

- [ ] **Step 4: Run full verification**

Run:

```bash
cargo fmt
cargo test
cargo build --release
```

Expected: PASS for all three commands

- [ ] **Step 5: Commit**

Run:

```bash
git add src/daemon.rs src/mcp.rs src/lib.rs
git commit -m "Extract daemon and MCP modules"
```

---

### Task 8: Final Cleanup, Docs, and Contributor Guardrails

**Files:**
- Modify: `README.md`
- Modify: `src/lib.rs`
- Modify: moved module files as needed

- [ ] **Step 1: Clean up visibility and imports**

For each module:

- remove accidental `pub` visibility that is only needed internally
- replace `pub use module::*` with explicit imports
- keep import groups stable and version-sorted
- avoid introducing wildcard imports

- [ ] **Step 2: Add a short contributor note to `README.md`**

Add a section like:

```markdown
## Internal Layout

- `src/main.rs` is the binary wrapper only
- `src/lib.rs` owns top-level dispatch
- subsystem modules live in focused files under `src/`
```

- [ ] **Step 3: Re-run the full suite after cleanup**

Run:

```bash
cargo fmt
cargo test
cargo build --release
git diff --check
```

Expected: PASS for all commands

- [ ] **Step 4: Commit**

Run:

```bash
git add README.md src/lib.rs src/*.rs src/telegram
git commit -m "Polish module boundaries after main refactor"
```

---

## Execution Notes

- Keep each extraction commit behavior-preserving. Do not rename commands, JSON fields, DB columns, or Telegram message text in this refactor.
- Move the tests that naturally belong to a module at the same time as the code. Do not leave all tests stranded in `src/lib.rs`.
- Prefer free functions over introducing service traits or object graphs. The current codebase is functional and explicit; keep that property.
- After Task 1, every subsequent task should shrink the largest remaining file. If a moved module still lands above roughly 2,000 lines, split it again before proceeding.
- If a task starts forcing circular imports, the boundary is wrong. Fix the module ownership instead of papering over it with broader visibility.

## Self-Review

- Spec coverage: this plan covers the thin binary requirement, library extraction, module tree design, explicit visibility, unit/integration test placement, and staged subsystem extraction.
- Placeholder scan: no `TODO`, `TBD`, or “write tests later” placeholders remain.
- Type consistency: the same module names and responsibilities are used consistently across the task list.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-04-15-main-rs-refactor.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
