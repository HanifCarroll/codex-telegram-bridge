use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture_path() -> Option<PathBuf> {
    env::var("CODEX_HERMES_BRIDGE_FIXTURE").ok().map(PathBuf::from)
}

fn fixture_actions_log_path() -> Option<PathBuf> {
    env::var("CODEX_HERMES_BRIDGE_ACTIONS_LOG").ok().map(PathBuf::from)
}

fn fixture_threads() -> Result<Option<Vec<Value>>> {
    let Some(path) = fixture_path() else {
        return Ok(None);
    };
    let parsed: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    Ok(Some(
        parsed
            .get("threads")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    ))
}

fn append_fixture_action(value: &Value) -> Result<()> {
    if let Some(path) = fixture_actions_log_path() {
        let mut existing = if path.exists() {
            fs::read_to_string(&path)?
        } else {
            String::new()
        };
        existing.push_str(&serde_json::to_string(value)?);
        existing.push('\n');
        fs::write(path, existing)?;
    }
    Ok(())
}

fn fixture_mode_enabled() -> bool {
    fixture_path().is_some()
}

fn load_fixture_into_db(conn: &Connection, now: u64) -> Result<bool> {
    let Some(threads) = fixture_threads()? else {
        return Ok(false);
    };
    for thread in threads {
        let snapshot = normalize_thread_snapshot(&thread, &thread)?;
        upsert_thread_snapshot(conn, &snapshot, now)?;
    }
    Ok(true)
}

fn fixture_threads_list(limit: u64) -> Result<Option<Value>> {
    let Some(threads) = fixture_threads()? else {
        return Ok(None);
    };
    let mut items = threads;
    items.sort_by_key(|thread| std::cmp::Reverse(thread.get("updatedAt").and_then(Value::as_u64).unwrap_or(0)));
    items.truncate(limit as usize);
    Ok(Some(json!({"data": items})))
}

fn fixture_show_thread(thread_id: &str) -> Result<Option<Value>> {
    let Some(threads) = fixture_threads()? else {
        return Ok(None);
    };
    Ok(threads
        .into_iter()
        .find(|thread| thread.get("id").and_then(Value::as_str) == Some(thread_id))
        .map(|thread| json!({"thread": thread})))
}

#[derive(Parser, Debug)]
#[command(name = "codex-hermes-bridge-rs")]
#[command(about = "Rust rewrite of the Codex Hermes bridge")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Diagnostics,
    Doctor {
        #[command(subcommand)]
        command: Option<DoctorCommands>,
    },
    Away {
        #[command(subcommand)]
        command: AwayCommands,
    },
    Threads {
        #[arg(long, default_value_t = 25)]
        limit: u64,
    },
    Follow {
        thread_id: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value_t = 3000)]
        duration: u64,
        #[arg(long = "poll-interval", default_value_t = 1000)]
        poll_interval: u64,
        #[arg(long)]
        events: Option<String>,
        #[arg(long = "experimental-realtime", default_value_t = false)]
        experimental_realtime: bool,
    },
    StatusAudit {
        thread_id: Option<String>,
    },
    Unarchive {
        thread_id: String,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    Brief {
        query: Vec<String>,
        #[arg(long, default_value_t = 3)]
        limit: u64,
    },
    Waiting {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: u64,
    },
    Inbox {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        attention: Option<String>,
        #[arg(long = "waiting-on")]
        waiting_on: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: u64,
    },
    Watch {
        #[arg(long, default_value_t = false)]
        once: bool,
        #[arg(long)]
        exec: Option<String>,
        #[arg(long)]
        events: Option<String>,
    },
    NotifyAway {
        #[arg(long, default_value_t = false)]
        completed: bool,
    },
    Sync {
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    Changes,
    Done,
    History {
        thread_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u64,
    },
    New {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        follow: bool,
        #[arg(long, default_value_t = false)]
        stream: bool,
        #[arg(long, default_value_t = 3000)]
        duration: u64,
        #[arg(long = "poll-interval", default_value_t = 1000)]
        poll_interval: u64,
        #[arg(long)]
        events: Option<String>,
        #[arg(long = "experimental-realtime", default_value_t = false)]
        experimental_realtime: bool,
        prompt: Vec<String>,
    },
    Fork {
        thread_id: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        follow: bool,
        #[arg(long, default_value_t = false)]
        stream: bool,
        #[arg(long, default_value_t = 3000)]
        duration: u64,
        #[arg(long = "poll-interval", default_value_t = 1000)]
        poll_interval: u64,
        #[arg(long)]
        events: Option<String>,
        #[arg(long = "experimental-realtime", default_value_t = false)]
        experimental_realtime: bool,
        prompt: Vec<String>,
    },
    Archive {
        #[arg(long = "thread-id")]
        thread_id_option: Option<String>,
        thread_ids: Vec<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        attention: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u64,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    Show {
        thread_id: String,
    },
    Reply {
        thread_id: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        follow: bool,
        #[arg(long, default_value_t = false)]
        stream: bool,
        #[arg(long, default_value_t = 3000)]
        duration: u64,
        #[arg(long = "poll-interval", default_value_t = 1000)]
        poll_interval: u64,
        #[arg(long)]
        events: Option<String>,
        #[arg(long = "experimental-realtime", default_value_t = false)]
        experimental_realtime: bool,
        prompt: Vec<String>,
    },
    Approve {
        thread_id: String,
        #[arg(long)]
        decision: Option<String>,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        follow: bool,
        #[arg(long, default_value_t = false)]
        stream: bool,
        #[arg(long, default_value_t = 3000)]
        duration: u64,
        #[arg(long = "poll-interval", default_value_t = 1000)]
        poll_interval: u64,
        #[arg(long)]
        events: Option<String>,
        #[arg(long = "experimental-realtime", default_value_t = false)]
        experimental_realtime: bool,
        positional_decision: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AwayCommands {
    On,
    Off,
    Status,
}

#[derive(Subcommand, Debug)]
enum DoctorCommands {
    Realtime {
        thread_id: String,
        #[arg(long = "experimental-realtime", default_value_t = false)]
        experimental_realtime: bool,
    },
}

#[derive(Serialize)]
struct ErrorEnvelope {
    ok: bool,
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

#[derive(Serialize)]
struct DiagnosticsEnvelope {
    ok: bool,
    diagnostics: Diagnostics,
}

#[derive(Serialize)]
struct Diagnostics {
    pid: u32,
    ppid: Option<u32>,
    cwd: String,
    args: Vec<String>,
    home: Option<String>,
    path: Option<String>,
    shell: Option<String>,
    term: Option<String>,
    codex_bin_env: Option<String>,
    bun_bin_env: Option<String>,
}

#[derive(Serialize)]
struct DoctorEnvelope {
    ok: bool,
    codex: DoctorCodex,
}

#[derive(Serialize)]
struct DoctorCodex {
    resolved_path: String,
    source: String,
    version_stdout: String,
}

#[derive(Serialize)]
struct ReplyResult<'a> {
    ok: bool,
    action: &'a str,
    dry_run: bool,
    thread_id: &'a str,
    message: &'a str,
    sent_at: u64,
}

#[derive(Serialize)]
struct ApproveResult<'a> {
    ok: bool,
    action: &'a str,
    dry_run: bool,
    thread_id: &'a str,
    decision: &'a str,
    sent_text: &'a str,
    sent_at: u64,
}

#[derive(Serialize)]
struct LiveReplyResult<'a> {
    ok: bool,
    action: &'a str,
    thread_id: &'a str,
    message: &'a str,
    sent_at: u64,
    resumed: Value,
    started: Value,
}

#[derive(Serialize)]
struct LiveApproveResult<'a> {
    ok: bool,
    action: &'a str,
    thread_id: &'a str,
    decision: &'a str,
    sent_text: &'a str,
    sent_at: u64,
    resumed: Value,
    started: Value,
}

#[derive(Debug, Clone)]
struct ResolvedBinary {
    path: PathBuf,
    source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct PendingPrompt {
    prompt_id: String,
    kind: String,
    status: String,
    question: Option<String>,
}

#[derive(Debug, Clone)]
struct BridgeThreadSnapshot {
    thread_id: String,
    name: Option<String>,
    cwd: Option<String>,
    updated_at: Option<u64>,
    status_type: String,
    status_flags: Vec<String>,
    last_turn_status: Option<String>,
    last_preview: Option<String>,
    pending_prompt: Option<PendingPrompt>,
}

#[derive(Debug, Clone, Serialize)]
struct WaitingThread {
    #[serde(rename = "threadId")]
    thread_id: String,
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: String,
    project: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<u64>,
    #[serde(rename = "statusType")]
    status_type: String,
    #[serde(rename = "statusFlags")]
    status_flags: Vec<String>,
    prompt: PendingPrompt,
    #[serde(rename = "lastPreview")]
    last_preview: Option<String>,
    label: String,
}

#[derive(Debug, Clone, Serialize)]
struct WaitingSummary {
    count: usize,
    #[serde(rename = "threadIds")]
    thread_ids: Vec<String>,
    labels: Vec<String>,
    #[serde(rename = "appliedFilters")]
    applied_filters: Value,
}

#[derive(Debug, Clone, Serialize)]
struct WaitingResult {
    summary: WaitingSummary,
    threads: Vec<WaitingThread>,
}

#[derive(Debug, Clone, Serialize)]
struct InboxItem {
    #[serde(rename = "threadId")]
    thread_id: String,
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: String,
    project: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<u64>,
    #[serde(rename = "ageSeconds")]
    age_seconds: Option<u64>,
    #[serde(rename = "statusType")]
    status_type: String,
    #[serde(rename = "statusFlags")]
    status_flags: Vec<String>,
    #[serde(rename = "lastPreview")]
    last_preview: Option<String>,
    #[serde(rename = "promptKind")]
    prompt_kind: Option<String>,
    #[serde(rename = "promptStatus")]
    prompt_status: Option<String>,
    question: Option<String>,
    basis: String,
    #[serde(rename = "attentionReason")]
    attention_reason: String,
    #[serde(rename = "waitingOn")]
    waiting_on: String,
    #[serde(rename = "suggestedAction")]
    suggested_action: String,
    priority: String,
    #[serde(rename = "recentAction")]
    recent_action: Option<Value>,
    label: String,
}

#[derive(Debug, Clone, Serialize)]
struct InboxSummary {
    total: usize,
    #[serde(rename = "needsAttention")]
    needs_attention: usize,
    #[serde(rename = "countsByReason")]
    counts_by_reason: Value,
    #[serde(rename = "appliedFilters")]
    applied_filters: Value,
}

#[derive(Debug, Clone, Serialize)]
struct InboxResult {
    summary: InboxSummary,
    items: Vec<InboxItem>,
}

fn main() {
    if let Err(error) = run() {
        let envelope = ErrorEnvelope {
            ok: false,
            error: ErrorBody {
                code: "internal_error",
                message: format!("{error:#}"),
            },
        };
        println!("{}", serde_json::to_string(&envelope).unwrap());
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Diagnostics => {
            let payload = DiagnosticsEnvelope {
                ok: true,
                diagnostics: Diagnostics {
                    pid: std::process::id(),
                    ppid: parent_pid(),
                    cwd: env::current_dir()?.display().to_string(),
                    args: env::args().collect(),
                    home: env::var("HOME").ok(),
                    path: env::var("PATH").ok(),
                    shell: env::var("SHELL").ok(),
                    term: env::var("TERM").ok(),
                    codex_bin_env: env::var("CODEX_BIN").ok(),
                    bun_bin_env: env::var("BUN_BIN").ok(),
                },
            };
            println!("{}", serde_json::to_string(&payload)?);
        }
        Commands::Doctor { command } => match command {
            None => {
                let resolved = resolve_codex_binary()?;
                let output = Command::new(&resolved.path)
                    .arg("--version")
                    .output()
                    .with_context(|| {
                        format!("failed to execute {} --version", resolved.path.display())
                    })?;
                if !output.status.success() {
                    bail!(
                        "codex binary {} returned non-zero exit status for --version",
                        resolved.path.display()
                    );
                }
                let payload = DoctorEnvelope {
                    ok: true,
                    codex: DoctorCodex {
                        resolved_path: resolved.path.display().to_string(),
                        source: resolved.source.to_string(),
                        version_stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    },
                };
                println!("{}", serde_json::to_string(&payload)?);
            }
            Some(DoctorCommands::Realtime {
                thread_id,
                experimental_realtime,
            }) => {
                let mut client = CodexAppServerClient::connect()?;
                let with_turns = client.request(
                    "thread/read",
                    json!({ "threadId": thread_id, "includeTurns": true }),
                );
                let without_turns = client.request(
                    "thread/read",
                    json!({ "threadId": thread_id, "includeTurns": false }),
                );
                let realtime_start = if experimental_realtime {
                    Some(client.request("realtime/start", json!({ "threadId": thread_id })))
                } else {
                    None
                };
                let realtime_stop = if experimental_realtime {
                    Some(client.request("realtime/stop", json!({ "threadId": thread_id })))
                } else {
                    None
                };
                println!(
                    "{}",
                    serde_json::to_string(&doctor_realtime_result(
                        &thread_id,
                        experimental_realtime,
                        with_turns,
                        without_turns,
                        realtime_start,
                        realtime_stop,
                    ))?
                );
            }
        },
        Commands::Away { command } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let payload = match command {
                AwayCommands::On => set_away_mode(&conn, true, now)?,
                AwayCommands::Off => set_away_mode(&conn, false, now)?,
                AwayCommands::Status => get_away_mode(&conn)?,
            };
            println!("{}", serde_json::to_string(&payload)?);
        }
        Commands::Threads { limit } => {
            if let Some(result) = fixture_threads_list(limit)? {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let result = client.request(
                    "thread/list",
                    json!({
                        "limit": limit,
                        "sortKey": "updated_at"
                    }),
                )?;
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Commands::Follow {
            thread_id,
            message,
            duration,
            poll_interval,
            events: _,
            experimental_realtime,
        } => {
            let mut client = CodexAppServerClient::connect()?;
            let initial = client.request(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": true }),
            ).ok().and_then(|value| value.get("thread").cloned());
            let started = match message.as_deref().map(str::trim).filter(|value| !value.is_empty()) {
                Some(text) => Some(client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [{ "type": "text", "text": text, "text_elements": [] }]
                    }),
                )?),
                None => None,
            };
            let events = build_follow_events(
                &thread_id,
                duration,
                poll_interval,
                experimental_realtime,
                initial,
                started,
                vec![],
            )?;
            println!("{}", serde_json::to_string(&json!({
                "ok": true,
                "action": "follow",
                "threadId": thread_id,
                "durationMs": duration,
                "experimentalRealtime": experimental_realtime,
                "events": events
            }))?);
        }
        Commands::StatusAudit { thread_id } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !load_fixture_into_db(&conn, now)? {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, 50)?;
            }
            println!("{}", serde_json::to_string(&get_status_audit(&conn, thread_id.as_deref())?)?);
        }
        Commands::Unarchive { thread_id, dry_run } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let live_result = if dry_run {
                None
            } else {
                let mut client = CodexAppServerClient::connect()?;
                Some(client.request("thread/unarchive", json!({ "threadId": thread_id }))?)
            };
            println!("{}", serde_json::to_string(&unarchive_thread_result(&conn, &thread_id, dry_run, now, live_result)?)?);
        }
        Commands::Brief { query, limit } => {
            let built = build_brief(&query.join(" "), limit)?;
            println!("{}", serde_json::to_string(&built)?);
        }
        Commands::Waiting { project, limit } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if let Some(threads) = fixture_threads()? {
                for thread in threads {
                    let snapshot = normalize_thread_snapshot(&thread, &thread)?;
                    upsert_thread_snapshot(&conn, &snapshot, now)?;
                }
            } else {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, limit.max(25))?;
            }
            let result = list_waiting_from_db(&conn, project.as_deref(), limit)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Inbox {
            project,
            status,
            attention,
            waiting_on,
            limit,
        } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !load_fixture_into_db(&conn, now)? {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, limit.max(25))?;
            }
            let result = list_inbox_from_db(
                &conn,
                now,
                project.as_deref(),
                status.as_deref(),
                attention.as_deref(),
                waiting_on.as_deref(),
                limit,
            )?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Watch { once, exec, events } => {
            let filter = events.as_deref().map(|value| {
                value
                    .split(',')
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .collect::<BTreeSet<_>>()
            });
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if once {
                let now = now_millis()?;
                if let Some(threads) = fixture_threads()? {
                    for thread in threads {
                        let snapshot = normalize_thread_snapshot(&thread, &thread)?;
                        upsert_thread_snapshot(&conn, &snapshot, now)?;
                    }
                } else {
                    let mut client = CodexAppServerClient::connect()?;
                    sync_state_from_live(&mut client, &conn, now, 50)?;
                }
                let filtered = filter_watch_events(watch_once_from_db(&conn)?, filter.as_ref());
                for event in &filtered {
                    if let Some(command) = exec.as_deref() {
                        run_exec_hook(command, event)?;
                    }
                }
                println!("{}", serde_json::to_string(&json!({ "events": filtered }))?);
            } else {
                println!("{}", serde_json::to_string(&json!({ "type": "watch_started", "away": get_away_mode(&conn)?["away"] }))?);
                let mut last = String::new();
                loop {
                    let now = now_millis()?;
                    let mut client = CodexAppServerClient::connect()?;
                    sync_state_from_live(&mut client, &conn, now, 50)?;
                    let filtered = filter_watch_events(watch_once_from_db(&conn)?, filter.as_ref());
                    let serialized = serde_json::to_string(&filtered)?;
                    if serialized != last {
                        last = serialized;
                        for event in filtered {
                            println!("{}", serde_json::to_string(&event)?);
                            if let Some(command) = exec.as_deref() {
                                run_exec_hook(command, &event)?;
                            }
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                }
            }
        }
        Commands::NotifyAway { completed } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let result = build_notify_away_from_db(&conn, now, completed)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Sync { limit } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit)?;
            println!("{}", serde_json::to_string(&json!({
                "ok": true,
                "action": "sync",
                "limit": limit,
                "syncedAt": now
            }))?);
        }
        Commands::Changes => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, 50)?;
            let result = list_changes_since_last_check(&conn, now)?;
            println!("{}", serde_json::to_string(&json!({
                "summary": {
                    "count": result.summary.total_count,
                    "lastCheckedAt": result.summary.last_checked_at,
                    "currentCursor": result.summary.current_cursor
                },
                "threads": result.threads
            }))?);
        }
        Commands::Done => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, 50)?;
            let result = list_done_since_last_check(&conn, now)?;
            println!("{}", serde_json::to_string(&json!({
                "summary": {
                    "count": result.summary.total_count,
                    "lastCheckedAt": result.summary.last_checked_at,
                    "currentCursor": result.summary.current_cursor
                },
                "threads": result.threads
            }))?);
        }
        Commands::History { thread_id, limit } => {
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let actions = get_thread_history(&conn, &thread_id, limit)?;
            println!("{}", serde_json::to_string(&json!({
                "threadId": thread_id,
                "actions": actions.iter().map(|action| json!({
                    "actionType": action.action_type,
                    "createdAt": action.created_at,
                    "payload": action.payload
                })).collect::<Vec<_>>()
            }))?);
        }
        Commands::New {
            cwd,
            message,
            dry_run,
            follow: _,
            stream: _,
            duration: _,
            poll_interval: _,
            events: _,
            experimental_realtime: _,
            prompt,
        } => {
            let message = message.or_else(|| {
                let joined = prompt.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            if dry_run {
                println!("{}", serde_json::to_string(&start_new_thread_dry_run(cwd.as_deref(), message.as_deref()))?);
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let created = client.request(
                    "thread/start",
                    json!({
                        "cwd": cwd,
                        "input": message.as_deref().map(|text| vec![json!({
                            "type": "text",
                            "text": text.trim(),
                            "text_elements": []
                        })])
                    }),
                )?;
                println!("{}", serde_json::to_string(&json!({
                    "ok": true,
                    "action": "new",
                    "cwd": cwd,
                    "message": message,
                    "created": created
                }))?);
            }
        }
        Commands::Fork {
            thread_id,
            message,
            dry_run,
            follow: _,
            stream: _,
            duration: _,
            poll_interval: _,
            events: _,
            experimental_realtime: _,
            prompt,
        } => {
            let message = message.or_else(|| {
                let joined = prompt.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            if dry_run {
                println!("{}", serde_json::to_string(&fork_thread_dry_run(&thread_id, message.as_deref()))?);
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let forked = client.request("thread/fork", json!({ "threadId": thread_id }))?;
                let new_thread_id = forked
                    .get("thread")
                    .and_then(|thread| thread.get("id"))
                    .and_then(Value::as_str)
                    .map(|value| value.to_string());
                let started = match (new_thread_id.as_deref(), message.as_deref()) {
                    (Some(new_thread_id), Some(message)) if !message.trim().is_empty() => Some(client.request(
                        "turn/start",
                        json!({
                            "threadId": new_thread_id,
                            "input": [{
                                "type": "text",
                                "text": message.trim(),
                                "text_elements": []
                            }]
                        }),
                    )?),
                    _ => None,
                };
                println!("{}", serde_json::to_string(&json!({
                    "ok": true,
                    "action": "fork",
                    "fromThreadId": thread_id,
                    "threadId": new_thread_id,
                    "message": message,
                    "forked": forked,
                    "started": started
                }))?);
            }
        }
        Commands::Archive {
            thread_id_option,
            thread_ids,
            project: _,
            status: _,
            attention,
            limit: _,
            dry_run,
            yes,
        } => {
            let mut targets = Vec::new();
            if let Some(raw) = thread_id_option {
                targets.extend(
                    raw.split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                );
            }
            targets.extend(thread_ids);
            let thread_id = targets.first().cloned();
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, 50)?;
            let preview = archive_from_db(
                &conn,
                thread_id.as_deref(),
                attention.as_deref(),
                dry_run,
                yes,
                now,
            )?;
            if dry_run || thread_id.is_none() {
                println!("{}", serde_json::to_string(&preview)?);
            } else {
                let target = thread_id.expect("thread_id should exist");
                let result = client.request("thread/archive", json!({ "threadId": target }))?;
                record_action(&conn, &target, "archive", json!({ "result": result, "archivedAt": now }), now)?;
                println!("{}", serde_json::to_string(&json!({
                    "ok": true,
                    "action": "archive",
                    "dryRun": false,
                    "results": [{
                        "threadId": target,
                        "status": "archived",
                        "result": result
                    }]
                }))?);
            }
        }
        Commands::Show { thread_id } => {
            if let Some(result) = fixture_show_thread(&thread_id)? {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let result = client.request(
                    "thread/read",
                    json!({
                        "threadId": thread_id,
                        "includeTurns": true
                    }),
                )?;
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Commands::Reply {
            thread_id,
            message,
            dry_run,
            follow: _,
            stream: _,
            duration: _,
            poll_interval: _,
            events: _,
            experimental_realtime: _,
            prompt,
        } => {
            let message = message
                .or_else(|| {
                    let joined = prompt.join(" ").trim().to_string();
                    (!joined.is_empty()).then_some(joined)
                })
                .unwrap_or_default()
                .trim()
                .to_string();
            if message.is_empty() {
                bail!("Reply message cannot be empty");
            }
            let sent_at = now_millis()?;
            if dry_run {
                let payload = ReplyResult {
                    ok: true,
                    action: "reply",
                    dry_run: true,
                    thread_id: &thread_id,
                    message: &message,
                    sent_at,
                };
                println!("{}", serde_json::to_string(&payload)?);
            } else if fixture_path().is_some() {
                let resumed = json!({"type": "resume", "threadId": thread_id});
                let started = json!({
                    "type": "turn/start",
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": message, "text_elements": []}]
                });
                append_fixture_action(&resumed)?;
                append_fixture_action(&started)?;
                let payload = LiveReplyResult {
                    ok: true,
                    action: "reply",
                    thread_id: &thread_id,
                    message: &message,
                    sent_at,
                    resumed,
                    started,
                };
                println!("{}", serde_json::to_string(&payload)?);
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
                let started = client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [{
                            "type": "text",
                            "text": message,
                            "text_elements": []
                        }]
                    }),
                )?;
                let db_path = state_db_path()?;
                let conn = create_state_db(&db_path)?;
                record_action(
                    &conn,
                    &thread_id,
                    "reply",
                    json!({
                        "message": message,
                        "resumed": resumed,
                        "started": started,
                        "sentAt": sent_at
                    }),
                    sent_at,
                )?;
                let payload = LiveReplyResult {
                    ok: true,
                    action: "reply",
                    thread_id: &thread_id,
                    message: &message,
                    sent_at,
                    resumed,
                    started,
                };
                println!("{}", serde_json::to_string(&payload)?);
            }
        }
        Commands::Approve {
            thread_id,
            decision,
            dry_run,
            follow: _,
            stream: _,
            duration: _,
            poll_interval: _,
            events: _,
            experimental_realtime: _,
            positional_decision,
        } => {
            let normalized = decision
                .or(positional_decision)
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            let sent_text = match normalized.as_str() {
                "approve" => "YES",
                "deny" => "NO",
                _ => bail!("Approval decision must be approve or deny"),
            };
            let sent_at = now_millis()?;
            if dry_run {
                let payload = ApproveResult {
                    ok: true,
                    action: "approve",
                    dry_run: true,
                    thread_id: &thread_id,
                    decision: &normalized,
                    sent_text,
                    sent_at,
                };
                println!("{}", serde_json::to_string(&payload)?);
            } else if fixture_path().is_some() {
                let resumed = json!({"type": "resume", "threadId": thread_id});
                let started = json!({
                    "type": "turn/start",
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": sent_text, "text_elements": []}]
                });
                append_fixture_action(&resumed)?;
                append_fixture_action(&started)?;
                let payload = LiveApproveResult {
                    ok: true,
                    action: "approve",
                    thread_id: &thread_id,
                    decision: &normalized,
                    sent_text,
                    sent_at,
                    resumed,
                    started,
                };
                println!("{}", serde_json::to_string(&payload)?);
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
                let started = client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [{
                            "type": "text",
                            "text": sent_text,
                            "text_elements": []
                        }]
                    }),
                )?;
                let db_path = state_db_path()?;
                let conn = create_state_db(&db_path)?;
                record_action(
                    &conn,
                    &thread_id,
                    "approve",
                    json!({
                        "decision": normalized,
                        "sentText": sent_text,
                        "resumed": resumed,
                        "started": started,
                        "sentAt": sent_at
                    }),
                    sent_at,
                )?;
                let payload = LiveApproveResult {
                    ok: true,
                    action: "approve",
                    thread_id: &thread_id,
                    decision: &normalized,
                    sent_text,
                    sent_at,
                    resumed,
                    started,
                };
                println!("{}", serde_json::to_string(&payload)?);
            }
        }
    }

    Ok(())
}

fn now_millis() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!(e))?
        .as_millis() as u64)
}

fn compact_text_preview(input: Option<String>, limit: usize) -> Option<String> {
    let text = input?.trim().replace(char::is_whitespace, " ");
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let count = normalized.chars().count();
    if count <= limit {
        return Some(normalized);
    }
    let truncated = normalized.chars().take(limit).collect::<String>();
    Some(format!("{}…", truncated.trim_end()))
}

fn derive_project_label(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?;
    Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|value| !value.is_empty())
}

fn derive_thread_display_name(
    name: Option<&str>,
    project: Option<&str>,
    question: Option<&str>,
    thread_id: &str,
) -> String {
    if let Some(value) = name.map(str::trim).filter(|value| !value.is_empty()) {
        return value.to_string();
    }
    if let Some(value) = question.map(str::trim).filter(|value| !value.is_empty()) {
        return compact_text_preview(Some(value.to_string()), 80)
            .unwrap_or_else(|| value.to_string());
    }
    if let Some(project) = project.filter(|value| !value.is_empty()) {
        return format!("Untitled {project} thread");
    }
    let short_id = thread_id.chars().take(8).collect::<String>();
    format!("Untitled thread {short_id}")
}

fn last_preview_from_thread(thread: &Value) -> Option<String> {
    let turns = thread.get("turns")?.as_array()?;
    let mut fallback: Option<String> = None;
    for turn in turns.iter().rev() {
        let Some(items) = turn.get("items").and_then(|value| value.as_array()) else {
            continue;
        };
        for item in items.iter().rev() {
            if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
                continue;
            }
            let Some(text) = item.get("text").and_then(Value::as_str).map(str::trim) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            if item.get("phase").and_then(Value::as_str) == Some("final_answer") {
                return Some(text.to_string());
            }
            if fallback.is_none() {
                fallback = Some(text.to_string());
            }
        }
    }
    fallback
}

fn derive_pending_prompt(
    thread_id: &str,
    status_flags: &[String],
    last_preview: Option<String>,
) -> Option<PendingPrompt> {
    if status_flags.iter().any(|flag| flag == "waitingOnApproval") {
        return Some(PendingPrompt {
            prompt_id: format!("approval:{thread_id}"),
            kind: "approval".to_string(),
            status: "Needs approval".to_string(),
            question: last_preview.or_else(|| Some("Approval required".to_string())),
        });
    }
    if status_flags
        .iter()
        .any(|flag| flag == "waitingOnUserInput" || flag == "waitingOnInput")
    {
        return Some(PendingPrompt {
            prompt_id: format!("reply:{thread_id}"),
            kind: "reply".to_string(),
            status: "Needs input".to_string(),
            question: last_preview.or_else(|| Some("Input required".to_string())),
        });
    }
    None
}

fn normalize_thread_snapshot(summary: &Value, thread: &Value) -> Result<BridgeThreadSnapshot> {
    let thread_id = thread
        .get("id")
        .or_else(|| summary.get("id"))
        .and_then(Value::as_str)
        .context("thread id missing")?
        .to_string();
    let name = thread
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            summary
                .get("name")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        });
    let cwd = thread
        .get("cwd")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .or_else(|| {
            summary
                .get("cwd")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        });
    let updated_at = thread
        .get("updatedAt")
        .and_then(Value::as_u64)
        .or_else(|| summary.get("updatedAt").and_then(Value::as_u64));
    let status_type = thread
        .pointer("/status/type")
        .and_then(Value::as_str)
        .or_else(|| summary.pointer("/status/type").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_string();
    let status_flags = thread
        .pointer("/status/activeFlags")
        .and_then(Value::as_array)
        .or_else(|| {
            summary
                .pointer("/status/activeFlags")
                .and_then(Value::as_array)
        })
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let turns = thread.get("turns").and_then(Value::as_array);
    let last_turn_status = turns
        .and_then(|items| items.last())
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let last_preview = last_preview_from_thread(thread);
    let pending_prompt = derive_pending_prompt(&thread_id, &status_flags, last_preview.clone());
    Ok(BridgeThreadSnapshot {
        thread_id,
        name,
        cwd,
        updated_at,
        status_type,
        status_flags,
        last_turn_status,
        last_preview,
        pending_prompt,
    })
}

fn classify_attention(snapshot: &BridgeThreadSnapshot) -> (&'static str, &'static str) {
    match snapshot
        .pending_prompt
        .as_ref()
        .map(|prompt| prompt.kind.as_str())
    {
        Some("approval") => ("pending_approval", "prompt_kind"),
        Some("reply") => ("needs_reply", "prompt_kind"),
        _ if snapshot.last_turn_status.as_deref() == Some("completed") => {
            ("completed", "last_turn_completed")
        }
        _ => ("active", "fallback_active"),
    }
}

fn classify_waiting_on(reason: &str) -> &'static str {
    match reason {
        "pending_approval" | "needs_reply" => "me",
        "active" => "codex",
        _ => "none",
    }
}

fn classify_suggested_action(reason: &str) -> &'static str {
    match reason {
        "pending_approval" => "approve",
        "needs_reply" => "reply",
        "completed" => "archive",
        _ => "inspect",
    }
}

fn classify_priority(reason: &str) -> &'static str {
    match reason {
        "pending_approval" => "high",
        "needs_reply" | "active" => "medium",
        _ => "low",
    }
}

fn score_inbox_item(item: &InboxItem) -> u128 {
    let priority_score = match item.priority.as_str() {
        "high" => 300_u128,
        "medium" => 200_u128,
        _ => 100_u128,
    };
    priority_score * 1_000_000_000_000 + item.updated_at.unwrap_or(0) as u128
}

fn classify_inbox_item(snapshot: &BridgeThreadSnapshot, now: u64) -> InboxItem {
    let (attention_reason, basis) = classify_attention(snapshot);
    let project = derive_project_label(snapshot.cwd.as_deref());
    let display_name = derive_thread_display_name(
        snapshot.name.as_deref(),
        project.as_deref(),
        snapshot
            .pending_prompt
            .as_ref()
            .and_then(|prompt| prompt.question.as_deref()),
        &snapshot.thread_id,
    );
    InboxItem {
        thread_id: snapshot.thread_id.clone(),
        name: snapshot.name.clone(),
        display_name: display_name.clone(),
        project: project.clone(),
        cwd: snapshot.cwd.clone(),
        updated_at: snapshot.updated_at,
        age_seconds: snapshot
            .updated_at
            .map(|updated| now.saturating_sub(updated)),
        status_type: snapshot.status_type.clone(),
        status_flags: snapshot.status_flags.clone(),
        last_preview: compact_text_preview(snapshot.last_preview.clone(), 220),
        prompt_kind: snapshot
            .pending_prompt
            .as_ref()
            .map(|prompt| prompt.kind.clone()),
        prompt_status: snapshot
            .pending_prompt
            .as_ref()
            .map(|prompt| prompt.status.clone()),
        question: compact_text_preview(
            snapshot
                .pending_prompt
                .as_ref()
                .and_then(|prompt| prompt.question.clone()),
            160,
        ),
        basis: basis.to_string(),
        attention_reason: attention_reason.to_string(),
        waiting_on: classify_waiting_on(attention_reason).to_string(),
        suggested_action: classify_suggested_action(attention_reason).to_string(),
        priority: classify_priority(attention_reason).to_string(),
        recent_action: None,
        label: format!(
            "{} · {}",
            display_name,
            project
                .clone()
                .or_else(|| snapshot.cwd.clone())
                .unwrap_or_else(|| "unknown cwd".to_string())
        ),
    }
}


#[derive(Debug, Clone)]
struct HistoryAction {
    action_type: String,
    payload: Value,
    created_at: u64,
}

#[derive(Debug, Clone)]
struct NotificationCandidate {
    delivery_key: String,
    kind: &'static str,
    thread_id: String,
    text: String,
}

#[derive(Debug, Clone)]
struct ChangesSummary {
    total_count: usize,
    last_checked_at: u64,
    current_cursor: u64,
}

#[derive(Debug, Clone)]
struct ChangesResult {
    summary: ChangesSummary,
    threads: Vec<Value>,
}

#[derive(Debug, Clone)]
struct DoneResult {
    summary: ChangesSummary,
    threads: Vec<Value>,
}

fn to_sql_i64(value: u64) -> Result<i64> {
    i64::try_from(value).context("timestamp out of range for sqlite i64")
}

fn from_sql_i64(value: i64) -> Result<u64> {
    u64::try_from(value).context("negative sqlite integer cannot be converted to u64")
}

fn optional_from_sql_i64(value: Option<i64>) -> Result<Option<u64>> {
    value.map(from_sql_i64).transpose()
}

fn create_state_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    init_state_db(&conn)?;
    Ok(conn)
}

#[cfg(test)]
fn create_state_db_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    init_state_db(&conn)?;
    Ok(conn)
}

fn init_state_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS settings (
          key TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS threads_cache (
          thread_id TEXT PRIMARY KEY,
          name TEXT,
          cwd TEXT,
          source TEXT,
          status_type TEXT NOT NULL,
          status_flags_json TEXT NOT NULL,
          updated_at INTEGER,
          last_seen_at INTEGER NOT NULL,
          last_turn_status TEXT,
          last_preview TEXT
        );
        CREATE TABLE IF NOT EXISTS pending_prompts (
          thread_id TEXT PRIMARY KEY,
          prompt_id TEXT NOT NULL,
          prompt_kind TEXT NOT NULL,
          prompt_status TEXT NOT NULL,
          question TEXT NOT NULL,
          created_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS delivery_log (
          event_key TEXT PRIMARY KEY,
          thread_id TEXT NOT NULL,
          event_type TEXT NOT NULL,
          delivered_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS notify_delivery_log (
          delivery_key TEXT PRIMARY KEY,
          away_session_id TEXT NOT NULL,
          thread_id TEXT NOT NULL,
          notification_type TEXT NOT NULL,
          delivered_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS actions_log (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          thread_id TEXT NOT NULL,
          action_type TEXT NOT NULL,
          payload_json TEXT NOT NULL,
          created_at INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn state_db_path() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    let dir = PathBuf::from(home).join(".codex-hermes-bridge");
    fs::create_dir_all(&dir)?;
    Ok(dir.join("state.db"))
}

fn get_setting_number(conn: &Connection, key: &str) -> Result<Option<u64>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.and_then(|value| value.parse::<u64>().ok()))
}

fn set_setting(conn: &Connection, key: &str, value: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value.to_string()],
    )?;
    Ok(())
}

fn upsert_thread_snapshot(conn: &Connection, snapshot: &BridgeThreadSnapshot, now: u64) -> Result<()> {
    conn.execute(
        "INSERT INTO threads_cache(
            thread_id, name, cwd, source, status_type, status_flags_json,
            updated_at, last_seen_at, last_turn_status, last_preview
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(thread_id) DO UPDATE SET
            name = excluded.name,
            cwd = excluded.cwd,
            source = excluded.source,
            status_type = excluded.status_type,
            status_flags_json = excluded.status_flags_json,
            updated_at = excluded.updated_at,
            last_seen_at = excluded.last_seen_at,
            last_turn_status = excluded.last_turn_status,
            last_preview = excluded.last_preview",
        params![
            snapshot.thread_id,
            snapshot.name,
            snapshot.cwd,
            "codex-app-server",
            snapshot.status_type,
            serde_json::to_string(&snapshot.status_flags)?,
            snapshot.updated_at.map(to_sql_i64).transpose()?,
            to_sql_i64(now)?,
            snapshot.last_turn_status,
            snapshot.last_preview,
        ],
    )?;

    match &snapshot.pending_prompt {
        Some(prompt) => {
            conn.execute(
                "INSERT INTO pending_prompts(thread_id, prompt_id, prompt_kind, prompt_status, question, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(thread_id) DO UPDATE SET
                    prompt_id = excluded.prompt_id,
                    prompt_kind = excluded.prompt_kind,
                    prompt_status = excluded.prompt_status,
                    question = excluded.question,
                    created_at = excluded.created_at",
                params![
                    snapshot.thread_id,
                    prompt.prompt_id,
                    prompt.kind,
                    prompt.status,
                    prompt.question.clone().unwrap_or_default(),
                    to_sql_i64(now)?,
                ],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM pending_prompts WHERE thread_id = ?1",
                params![snapshot.thread_id],
            )?;
        }
    }
    Ok(())
}

fn record_action(
    conn: &Connection,
    thread_id: &str,
    action_type: &str,
    payload: Value,
    now: u64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO actions_log(thread_id, action_type, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            thread_id,
            action_type,
            serde_json::to_string(&payload)?,
            to_sql_i64(now)?
        ],
    )?;
    Ok(())
}

fn get_thread_history(
    conn: &Connection,
    thread_id: &str,
    limit: u64,
) -> Result<Vec<HistoryAction>> {
    let mut stmt = conn.prepare(
        "SELECT action_type, payload_json, created_at
         FROM actions_log
         WHERE thread_id = ?1
         ORDER BY id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![thread_id, to_sql_i64(limit)?], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(|(action_type, payload_json, created_at)| {
            Ok(HistoryAction {
                action_type,
                payload: serde_json::from_str::<Value>(&payload_json).unwrap_or(Value::Null),
                created_at: from_sql_i64(created_at)?,
            })
        })
        .collect()
}

fn list_waiting_from_db(
    conn: &Connection,
    project_filter: Option<&str>,
    limit: u64,
) -> Result<WaitingResult> {
    let mut stmt = conn.prepare(
        "SELECT t.thread_id, t.name, t.cwd, t.updated_at, t.status_type, t.status_flags_json,
                t.last_preview, p.prompt_id, p.prompt_kind, p.prompt_status, p.question
         FROM pending_prompts p
         INNER JOIN threads_cache t ON t.thread_id = p.thread_id
         ORDER BY COALESCE(t.updated_at, 0) DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
        ))
    })?;
    let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut threads = raw
        .into_iter()
        .map(
            |(
                thread_id,
                name,
                cwd,
                updated_at_raw,
                status_type,
                status_flags_json,
                last_preview,
                prompt_id,
                prompt_kind,
                prompt_status,
                question,
            )| {
                let prompt = PendingPrompt {
                    prompt_id,
                    kind: prompt_kind.clone(),
                    status: prompt_status,
                    question: Some(question.clone()),
                };
                let project = derive_project_label(cwd.as_deref());
                let display_name = derive_thread_display_name(
                    name.as_deref(),
                    project.as_deref(),
                    Some(question.as_str()),
                    &thread_id,
                );
                let label = format!(
                    "{} · {} · {}",
                    display_name,
                    project
                        .clone()
                        .or(cwd.clone())
                        .unwrap_or_else(|| "unknown cwd".to_string()),
                    prompt_kind
                );
                Ok(WaitingThread {
                    thread_id,
                    name,
                    display_name,
                    project,
                    cwd,
                    updated_at: optional_from_sql_i64(updated_at_raw)?,
                    status_type,
                    status_flags: serde_json::from_str(&status_flags_json).unwrap_or_default(),
                    prompt,
                    last_preview,
                    label,
                })
            },
        )
        .collect::<Result<Vec<_>>>()?;
    if let Some(needle) = project_filter {
        let needle = needle.to_lowercase();
        threads.retain(|thread| {
            thread
                .cwd
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&needle)
        });
    }
    if limit > 0 {
        threads.truncate(limit as usize);
    }
    Ok(WaitingResult {
        summary: WaitingSummary {
            count: threads.len(),
            thread_ids: threads.iter().map(|thread| thread.thread_id.clone()).collect(),
            labels: threads.iter().map(|thread| thread.label.clone()).collect(),
            applied_filters: json!({ "project": project_filter, "limit": limit }),
        },
        threads,
    })
}

fn list_changes_since_last_check(conn: &Connection, now: u64) -> Result<ChangesResult> {
    let last_checked_at = get_setting_number(conn, "changes_last_checked_at")?.unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT thread_id, name, cwd, updated_at, status_type, status_flags_json, last_preview
         FROM threads_cache
         WHERE COALESCE(updated_at, 0) > ?1
         ORDER BY COALESCE(updated_at, 0) DESC",
    )?;
    let rows = stmt.query_map(params![to_sql_i64(last_checked_at)?], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let threads = raw
        .into_iter()
        .map(|(thread_id, name, cwd, updated_at_raw, status_type, status_flags_json, last_preview)| {
            Ok(json!({
                "threadId": thread_id,
                "name": name,
                "cwd": cwd,
                "updatedAt": optional_from_sql_i64(updated_at_raw)?,
                "statusType": status_type,
                "statusFlags": serde_json::from_str::<Value>(&status_flags_json).unwrap_or(json!([])),
                "lastPreview": compact_text_preview(last_preview, 220),
                "changeType": "updated"
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    set_setting(conn, "changes_last_checked_at", now)?;
    Ok(ChangesResult {
        summary: ChangesSummary {
            total_count: threads.len(),
            last_checked_at,
            current_cursor: now,
        },
        threads,
    })
}

fn list_done_since_last_check(conn: &Connection, now: u64) -> Result<DoneResult> {
    let last_checked_at = get_setting_number(conn, "done_last_checked_at")?.unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT thread_id, name, cwd, updated_at, last_turn_status, last_preview
         FROM threads_cache
         WHERE COALESCE(updated_at, 0) > ?1
           AND last_turn_status = 'completed'
         ORDER BY COALESCE(updated_at, 0) DESC",
    )?;
    let rows = stmt.query_map(params![to_sql_i64(last_checked_at)?], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let threads = raw
        .into_iter()
        .map(|(thread_id, name, cwd, updated_at_raw, last_turn_status, last_preview)| {
            let project = derive_project_label(cwd.as_deref());
            Ok(json!({
                "threadId": thread_id,
                "name": name,
                "displayName": derive_thread_display_name(name.as_deref(), project.as_deref(), None, &thread_id),
                "project": project,
                "cwd": cwd,
                "updatedAt": optional_from_sql_i64(updated_at_raw)?,
                "basis": "last_turn_completed",
                "lastTurnStatus": last_turn_status,
                "finalSummary": compact_text_preview(last_preview, 240)
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    set_setting(conn, "done_last_checked_at", now)?;
    Ok(DoneResult {
        summary: ChangesSummary {
            total_count: threads.len(),
            last_checked_at,
            current_cursor: now,
        },
        threads,
    })
}

fn list_inbox_from_db(
    conn: &Connection,
    now: u64,
    project_filter: Option<&str>,
    status_filter: Option<&str>,
    attention_filter: Option<&str>,
    waiting_on_filter: Option<&str>,
    limit: u64,
) -> Result<InboxResult> {
    let mut stmt = conn.prepare(
        "SELECT t.thread_id, t.name, t.cwd, t.updated_at, t.status_type, t.status_flags_json,
                t.last_turn_status, t.last_preview, p.prompt_kind, p.prompt_status, p.question
         FROM threads_cache t
         LEFT JOIN pending_prompts p ON p.thread_id = t.thread_id
         ORDER BY COALESCE(t.updated_at, 0) DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, Option<String>>(10)?,
        ))
    })?;
    let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = raw
        .into_iter()
        .map(
            |(
                thread_id,
                name,
                cwd,
                updated_at_raw,
                status_type,
                status_flags_json,
                last_turn_status,
                last_preview,
                prompt_kind,
                prompt_status,
                question,
            )| {
                let status_flags = serde_json::from_str::<Vec<String>>(&status_flags_json)
                    .unwrap_or_default();
                let pending_prompt = match prompt_kind.clone() {
                    Some(kind) => Some(PendingPrompt {
                        prompt_id: format!("{}:{}", kind, thread_id),
                        kind,
                        status: prompt_status.unwrap_or_else(|| "Needs input".to_string()),
                        question,
                    }),
                    None => None,
                };
                let snapshot = BridgeThreadSnapshot {
                    thread_id,
                    name,
                    cwd,
                    updated_at: optional_from_sql_i64(updated_at_raw)?,
                    status_type,
                    status_flags,
                    last_turn_status,
                    last_preview,
                    pending_prompt,
                };
                Ok(classify_inbox_item(&snapshot, now))
            },
        )
        .collect::<Result<Vec<_>>>()?;

    if let Some(needle) = project_filter {
        let needle = needle.to_lowercase();
        items.retain(|item| {
            item.cwd
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&needle)
        });
    }
    if let Some(status) = status_filter {
        items.retain(|item| item.status_type == status);
    }
    if let Some(attention) = attention_filter {
        items.retain(|item| item.attention_reason == attention);
    }
    if let Some(waiting_on) = waiting_on_filter {
        items.retain(|item| item.waiting_on == waiting_on);
    }

    items.sort_by(|left, right| score_inbox_item(right).cmp(&score_inbox_item(left)));
    if limit > 0 {
        items.truncate(limit as usize);
    }

    let pending_approval = items
        .iter()
        .filter(|item| item.attention_reason == "pending_approval")
        .count();
    let needs_reply = items
        .iter()
        .filter(|item| item.attention_reason == "needs_reply")
        .count();
    let active = items
        .iter()
        .filter(|item| item.attention_reason == "active")
        .count();
    let completed = items
        .iter()
        .filter(|item| item.attention_reason == "completed")
        .count();

    Ok(InboxResult {
        summary: InboxSummary {
            total: items.len(),
            needs_attention: items.iter().filter(|item| item.waiting_on == "me").count(),
            counts_by_reason: json!({
                "pendingApproval": pending_approval,
                "needsReply": needs_reply,
                "active": active,
                "completed": completed
            }),
            applied_filters: json!({
                "project": project_filter,
                "status": status_filter,
                "attention": attention_filter,
                "waitingOn": waiting_on_filter,
                "limit": limit
            }),
        },
        items,
    })
}

fn start_new_thread_dry_run(cwd: Option<&str>, message: Option<&str>) -> Value {
    json!({
        "ok": true,
        "action": "new",
        "dryRun": true,
        "cwd": cwd,
        "message": message.map(str::trim).filter(|value| !value.is_empty())
    })
}

fn fork_thread_dry_run(thread_id: &str, message: Option<&str>) -> Value {
    json!({
        "ok": true,
        "action": "fork",
        "dryRun": true,
        "fromThreadId": thread_id,
        "message": message.map(str::trim).filter(|value| !value.is_empty())
    })
}

fn archive_from_db(
    conn: &Connection,
    thread_id: Option<&str>,
    attention: Option<&str>,
    dry_run: bool,
    yes: bool,
    now: u64,
) -> Result<Value> {
    let targets = if let Some(thread_id) = thread_id {
        vec![thread_id.to_string()]
    } else {
        let inbox = list_inbox_from_db(conn, now, None, None, attention, None, 100)?;
        inbox.items.into_iter().map(|item| item.thread_id).collect::<Vec<_>>()
    };

    let using_filter_selection = thread_id.is_none() && attention.is_some();
    if !dry_run && using_filter_selection && !yes {
        bail!("Refusing bulk archive without --yes or --dry-run");
    }

    let results = targets
        .into_iter()
        .map(|thread_id| {
            json!({
                "threadId": thread_id,
                "status": if dry_run { "would_archive" } else { "archived" }
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "ok": true,
        "action": "archive",
        "dryRun": dry_run,
        "results": results
    }))
}

fn sync_state_from_live(client: &mut CodexAppServerClient, conn: &Connection, now: u64, limit: u64) -> Result<()> {
    let list = client.request(
        "thread/list",
        json!({
            "limit": limit,
            "sortKey": "updated_at"
        }),
    )?;
    let summaries = list
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for summary in summaries {
        let thread_id = summary
            .get("id")
            .and_then(Value::as_str)
            .context("thread id missing in thread/list")?;
        let read = client.request(
            "thread/read",
            json!({
                "threadId": thread_id,
                "includeTurns": true
            }),
        )?;
        let Some(thread) = read.get("thread") else {
            continue;
        };
        let snapshot = normalize_thread_snapshot(&summary, thread)?;
        upsert_thread_snapshot(conn, &snapshot, now)?;
    }
    Ok(())
}

fn parent_pid() -> Option<u32> {
    #[cfg(target_family = "unix")]
    {
        let status = fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("PPid:\t") {
                return value.trim().parse::<u32>().ok();
            }
        }
        None
    }
    #[cfg(not(target_family = "unix"))]
    {
        None
    }
}

fn resolve_codex_binary() -> Result<ResolvedBinary> {
    let cwd = env::current_dir()?;
    let mut seen = BTreeSet::new();
    let mut candidates: Vec<ResolvedBinary> = Vec::new();

    if let Ok(override_path) = env::var("CODEX_BIN") {
        push_candidate(
            &mut candidates,
            &mut seen,
            PathBuf::from(override_path),
            "override",
        );
    }

    for root in workspace_roots(&cwd) {
        push_candidate(
            &mut candidates,
            &mut seen,
            root.join("node_modules").join(".bin").join("codex"),
            "workspace-local",
        );
    }

    if let Ok(path_codex) = which::which("codex") {
        push_candidate(&mut candidates, &mut seen, path_codex, "path");
    }

    if let Some(home) = dirs::home_dir() {
        push_candidate(
            &mut candidates,
            &mut seen,
            home.join("Applications/Codex.app/Contents/Resources/codex"),
            "platform-known",
        );
    }
    push_candidate(
        &mut candidates,
        &mut seen,
        PathBuf::from("/Applications/Codex.app/Contents/Resources/codex"),
        "platform-known",
    );

    for candidate in candidates {
        if candidate.path.is_file() {
            return Ok(candidate);
        }
    }

    bail!(
        "Could not resolve codex executable. Set CODEX_BIN or install Codex so `codex --version` works in this environment"
    )
}

fn push_candidate(
    candidates: &mut Vec<ResolvedBinary>,
    seen: &mut BTreeSet<String>,
    path: PathBuf,
    source: &'static str,
) {
    let key = path.display().to_string();
    if seen.insert(key) {
        candidates.push(ResolvedBinary { path, source });
    }
}

fn workspace_roots(start: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut current = start.to_path_buf();
    loop {
        roots.push(current.clone());
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => break,
        }
    }
    roots
}

fn set_setting_text(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn get_setting_text(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn render_notification_message(
    kind: &str,
    display_name: &str,
    project: &str,
    cwd: Option<&str>,
    summary: Option<&str>,
    detail: Option<&str>,
    recent_action: Option<&str>,
    next_step: Option<&str>,
) -> String {
    let type_label = match kind {
        "thread_waiting" => "waiting",
        "thread_completed" => "completed",
        _ => "updated",
    };
    let mut lines = vec![
        "🔔 Codex update".to_string(),
        format!("• Type: {type_label}"),
        format!("• Project: {project}"),
    ];
    if let Some(cwd) = cwd.filter(|value| !value.trim().is_empty()) {
        lines.push(format!("• Path: {}", cwd.trim()));
    }
    lines.push(String::new());
    lines.push(format!("• Thread: {display_name}"));
    if let Some(summary) = summary.filter(|value| !value.trim().is_empty()) {
        lines.push(String::new());
        lines.push(if kind == "thread_waiting" {
            "❓ Question".to_string()
        } else {
            "📝 Summary".to_string()
        });
        lines.push(summary.to_string());
    }
    if let Some(detail) = detail.filter(|value| !value.trim().is_empty()) {
        lines.push(String::new());
        lines.push("ℹ️ Details".to_string());
        lines.push(detail.to_string());
    }
    if let Some(recent_action) = recent_action.filter(|value| !value.trim().is_empty()) {
        lines.push(String::new());
        lines.push("🕘 Last action".to_string());
        lines.push(recent_action.replace("Last action: ", ""));
    }
    if let Some(next_step) = next_step.filter(|value| !value.trim().is_empty()) {
        lines.push(String::new());
        lines.push("➡️ Next".to_string());
        lines.push(next_step.to_string());
    }
    lines.join("\n")
}

fn get_away_mode(conn: &Connection) -> Result<Value> {
    let away_started_at = get_setting_number(conn, "away_started_at")?;
    if get_setting_text(conn, "away")?.is_none() {
        if let Some(legacy) = get_setting_text(conn, "away_mode")? {
            set_setting_text(conn, "away", &legacy)?;
            conn.execute("DELETE FROM settings WHERE key = ?1", params!["away_mode"])?;
        }
    }
    let existing_session = get_setting_text(conn, "away_session_id")?;
    let away_session_id = existing_session.or_else(|| away_started_at.map(|value| value.to_string()));
    if let Some(session_id) = away_session_id.as_deref() {
        if get_setting_text(conn, "away_session_id")?.is_none() {
            conn.execute(
                "INSERT INTO settings(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params!["away_session_id", session_id],
            )?;
        }
    }
    Ok(json!({
        "ok": true,
        "away": get_setting_text(conn, "away")?.unwrap_or_default() == "true",
        "awayStartedAt": away_started_at,
        "awaySessionId": away_session_id
    }))
}

fn set_away_mode(conn: &Connection, away: bool, now: u64) -> Result<Value> {
    set_setting_text(conn, "away", if away { "true" } else { "false" })?;
    conn.execute("DELETE FROM settings WHERE key = ?1", params!["away_mode"])?;

    if away {
        let session_id = now.to_string();
        set_setting(conn, "away_started_at", now)?;
        conn.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params!["away_session_id", session_id],
        )?;
        Ok(json!({
            "ok": true,
            "away": true,
            "awayStartedAt": now,
            "awaySessionId": now.to_string()
        }))
    } else {
        conn.execute("DELETE FROM settings WHERE key = ?1", params!["away_started_at"])?;
        conn.execute("DELETE FROM settings WHERE key = ?1", params!["away_session_id"])?;
        Ok(json!({
            "ok": true,
            "away": false,
            "awayStartedAt": Value::Null,
            "awaySessionId": Value::Null
        }))
    }
}

fn unarchive_thread_result(
    conn: &Connection,
    thread_id: &str,
    dry_run: bool,
    now: u64,
    live_result: Option<Value>,
) -> Result<Value> {
    if dry_run {
        return Ok(json!({
            "ok": true,
            "action": "unarchive",
            "dryRun": true,
            "results": [{ "threadId": thread_id, "status": "would_unarchive" }]
        }));
    }

    let result = live_result.unwrap_or_else(|| json!({ "threadId": thread_id, "ok": true }));
    record_action(
        conn,
        thread_id,
        "unarchive",
        json!({ "result": result, "unarchivedAt": now }),
        now,
    )?;

    Ok(json!({
        "ok": true,
        "action": "unarchive",
        "dryRun": false,
        "results": [{ "threadId": thread_id, "status": "unarchived", "result": result }]
    }))
}

fn get_status_audit(conn: &Connection, thread_id: Option<&str>) -> Result<Value> {
    let inbox = list_inbox_from_db(conn, now_millis()?, None, None, None, None, 1000)?;
    let audits = inbox
        .items
        .into_iter()
        .filter(|item| thread_id.map(|target| item.thread_id == target).unwrap_or(true))
        .map(|item| {
            json!({
                "threadId": item.thread_id,
                "name": item.name,
                "displayName": item.display_name,
                "project": item.project,
                "cwd": item.cwd,
                "raw": {
                    "statusType": item.status_type,
                    "statusFlags": item.status_flags,
                    "lastTurnStatus": Value::Null,
                    "promptKind": item.prompt_kind,
                    "promptStatus": item.prompt_status
                },
                "derived": {
                    "basis": item.basis,
                    "attentionReason": item.attention_reason,
                    "waitingOn": item.waiting_on,
                    "suggestedAction": item.suggested_action
                }
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "audits": audits }))
}

fn step_result(label: &str, result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({ "ok": true, "step": label, "result": value }),
        Err(error) => json!({
            "ok": false,
            "step": label,
            "error": {
                "message": format!("{error:#}")
            }
        }),
    }
}

fn doctor_realtime_result(
    thread_id: &str,
    experimental_api: bool,
    read_with_turns: Result<Value>,
    read_without_turns: Result<Value>,
    realtime_start: Option<Result<Value>>,
    realtime_stop: Option<Result<Value>>,
) -> Value {
    json!({
        "ok": true,
        "action": "doctor-realtime",
        "threadId": thread_id,
        "experimentalApi": experimental_api,
        "checks": {
            "readWithTurns": step_result("read_with_turns", read_with_turns),
            "readWithoutTurns": step_result("read_without_turns", read_without_turns),
            "realtimeStart": realtime_start.map(|value| step_result("realtime_start", value)),
            "realtimeStop": realtime_stop.map(|value| step_result("realtime_stop", value))
        }
    })
}

fn build_follow_events(
    thread_id: &str,
    duration_ms: u64,
    poll_interval_ms: u64,
    experimental_realtime: bool,
    initial_thread: Option<Value>,
    started: Option<Value>,
    notifications: Vec<Value>,
) -> Result<Vec<Value>> {
    let mut events = vec![json!({
        "type": "follow_started",
        "threadId": thread_id,
        "durationMs": duration_ms,
        "pollIntervalMs": poll_interval_ms,
        "experimentalRealtime": experimental_realtime
    })];
    if let Some(thread) = initial_thread {
        events.push(json!({
            "type": "follow_snapshot",
            "threadId": thread_id,
            "thread": thread
        }));
    }
    if let Some(started) = started {
        events.push(json!({
            "type": "follow_turn_started",
            "threadId": thread_id,
            "started": started
        }));
    }
    events.extend(notifications);
    Ok(events)
}

fn filter_watch_events(events: Vec<Value>, filter: Option<&BTreeSet<String>>) -> Vec<Value> {
    match filter {
        None => events,
        Some(filter) => events
            .into_iter()
            .filter(|event| {
                event
                    .get("type")
                    .and_then(Value::as_str)
                    .map(|kind| filter.contains(kind))
                    .unwrap_or(false)
            })
            .collect(),
    }
}

fn run_exec_hook(command: &str, event: &Value) -> Result<()> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn exec hook: {command}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(serde_json::to_string(event)?.as_bytes())?;
    }
    let _ = child.wait();
    Ok(())
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|part| part.len() >= 3)
        .map(|part| part.to_string())
        .collect()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_text(value: &str, max_length: usize) -> String {
    let trimmed = normalize_whitespace(value);
    if trimmed.chars().count() <= max_length {
        trimmed
    } else {
        format!("{}…", trimmed.chars().take(max_length.saturating_sub(1)).collect::<String>())
    }
}

fn session_file_path(home: &Path, session_id: &str, updated_at: &str) -> Option<PathBuf> {
    let date = updated_at.get(0..10)?;
    let parts = date.split('-').collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let folder = home
        .join(".codex")
        .join("sessions")
        .join(parts[0])
        .join(parts[1])
        .join(parts[2]);
    if !folder.exists() {
        return None;
    }
    let suffix = format!("-{session_id}.jsonl");
    std::fs::read_dir(folder)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(&suffix))
                .unwrap_or(false)
        })
}

fn build_brief(query: &str, limit: u64) -> Result<Value> {
    let query = query.trim();
    if query.is_empty() {
        bail!("Missing query");
    }
    let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
    let index_path = home.join(".codex/session_index.jsonl");
    if !index_path.exists() {
        return Ok(json!({ "query": query, "sessions": [] }));
    }

    let tokens = tokenize_query(query);
    let index = fs::read_to_string(index_path)?;
    let mut sessions = Vec::new();
    for line in index.lines().filter(|line| !line.trim().is_empty()) {
        let entry: Value = serde_json::from_str(line)?;
        let session_id = match entry.get("id").and_then(Value::as_str) {
            Some(value) => value,
            None => continue,
        };
        let updated_at = match entry.get("updated_at").and_then(Value::as_str) {
            Some(value) => value,
            None => continue,
        };
        let Some(path) = session_file_path(&home, session_id, updated_at) else {
            continue;
        };
        let content = fs::read_to_string(path)?;
        let mut score = 0u64;
        let mut receipts = Vec::new();
        let mut cwd: Option<String> = None;
        let mut meta_timestamp: Option<String> = None;
        let mut user_asks = Vec::new();
        let mut why_relevant = Vec::new();
        let mut decisions = Vec::new();
        for (idx, row) in content.lines().enumerate() {
            if row.trim().is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(row)?;
            if parsed.get("type").and_then(Value::as_str) == Some("session_meta") {
                cwd = parsed.pointer("/payload/cwd").and_then(Value::as_str).map(|s| s.to_string());
                meta_timestamp = parsed.pointer("/payload/timestamp").and_then(Value::as_str).map(|s| s.to_string());
                continue;
            }
            if parsed.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let role = parsed.pointer("/payload/role").and_then(Value::as_str).unwrap_or("");
            let pieces = parsed.pointer("/payload/content").and_then(Value::as_array).cloned().unwrap_or_default();
            let text = pieces
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let normalized = normalize_whitespace(&text);
            if normalized.is_empty() {
                continue;
            }
            let lower = normalized.to_lowercase();
            let line_score = tokens.iter().filter(|token| lower.contains(token.as_str())).count() as u64;
            if line_score == 0 {
                continue;
            }
            score += line_score;
            receipts.push(json!({
                "source": format!("{}:{}", session_id, idx + 1),
                "line": idx + 1,
                "role": role,
                "snippet": truncate_text(&normalized, 220)
            }));
            if role == "user" {
                user_asks.push(truncate_text(&normalized, 180));
            } else {
                why_relevant.push(truncate_text(&normalized, 180));
                if normalized.contains("should") || normalized.contains("need") || normalized.contains("verified") {
                    decisions.push(truncate_text(&normalized, 180));
                }
            }
        }
        if score == 0 {
            continue;
        }
        sessions.push((score, json!({
            "session_id": session_id,
            "thread_name": entry.get("thread_name").cloned().unwrap_or(Value::Null),
            "cwd": cwd,
            "updated_at": meta_timestamp.or_else(|| Some(updated_at.to_string())),
            "why_relevant": why_relevant.into_iter().take(3).collect::<Vec<_>>(),
            "user_asks": user_asks.into_iter().take(3).collect::<Vec<_>>(),
            "decisions": decisions.into_iter().take(3).collect::<Vec<_>>(),
            "receipts": receipts.into_iter().take(3).collect::<Vec<_>>(),
            "suggested_show_command": format!("codex-recall show {session_id} --limit 40")
        })));
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    let limited = sessions
        .into_iter()
        .take(limit.clamp(1, 3) as usize)
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    Ok(json!({ "query": query, "sessions": limited }))
}

fn build_notify_away_from_db(conn: &Connection, now: u64, include_completed: bool) -> Result<Value> {
    let away = get_setting_text(conn, "away_mode")?.unwrap_or_default() == "true";
    let away_session_id = get_setting_text(conn, "away_session_id")?;
    let away_started_at = get_setting_number(conn, "away_started_at")?.unwrap_or(0);
    if !away || away_session_id.is_none() {
        return Ok(json!({ "ok": true, "action": "notify-away", "notifications": [] }));
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS notify_delivery_log (
            delivery_key TEXT PRIMARY KEY,
            away_session_id TEXT NOT NULL,
            thread_id TEXT NOT NULL,
            notification_type TEXT NOT NULL,
            delivered_at INTEGER NOT NULL
        );",
    )?;

    let waiting = list_waiting_from_db(conn, None, 100)?;
    let inbox = list_inbox_from_db(conn, now, None, None, None, None, 100)?;
    let away_session_id = away_session_id.unwrap();
    let mut candidates = Vec::new();

    for thread in waiting.threads {
        let delivery_key = format!(
            "{}:thread_waiting:{}:{}:{}",
            away_session_id,
            thread.thread_id,
            thread.updated_at.unwrap_or(0),
            thread.prompt.question.clone().unwrap_or_default()
        );
        candidates.push(NotificationCandidate {
            delivery_key,
            kind: "thread_waiting",
            thread_id: thread.thread_id.clone(),
            text: render_notification_message(
                "thread_waiting",
                &thread.display_name,
                thread.project.as_deref().unwrap_or("unknown"),
                thread.cwd.as_deref(),
                thread.prompt.question.as_deref(),
                Some(&thread.prompt.kind),
                None,
                Some("Tell me how you want me to reply"),
            ),
        });
    }

    for item in inbox.items {
        if item.updated_at.unwrap_or(0) < away_started_at {
            continue;
        }
        if item.attention_reason == "completed" && !include_completed {
            continue;
        }
        if item.attention_reason == "needs_reply" || item.attention_reason == "pending_approval" {
            continue;
        }
        let kind = if item.attention_reason == "completed" {
            "thread_completed"
        } else {
            "thread_updated"
        };
        let delivery_key = format!(
            "{}:{}:{}:{}",
            away_session_id,
            kind,
            item.thread_id,
            item.updated_at.unwrap_or(0)
        );
        candidates.push(NotificationCandidate {
            delivery_key,
            kind,
            thread_id: item.thread_id.clone(),
            text: render_notification_message(
                kind,
                &item.display_name,
                item.project.as_deref().unwrap_or("unknown"),
                item.cwd.as_deref(),
                item.last_preview.as_deref(),
                None,
                None,
                Some(if kind == "thread_completed" {
                    "Tell me if you want me to follow up"
                } else {
                    "Tell me if you want me to check it"
                }),
            ),
        });
    }

    let mut notifications = Vec::new();
    for candidate in candidates {
        let exists: Option<String> = conn
            .query_row(
                "SELECT delivery_key FROM notify_delivery_log WHERE away_session_id = ?1 AND delivery_key = ?2",
                params![away_session_id, candidate.delivery_key],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_some() {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO notify_delivery_log(delivery_key, away_session_id, thread_id, notification_type, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                candidate.delivery_key,
                away_session_id,
                candidate.thread_id,
                candidate.kind,
                to_sql_i64(now)?
            ],
        )?;
        notifications.push(json!({
            "type": candidate.kind,
            "threadId": candidate.thread_id,
            "text": candidate.text
        }));
    }

    Ok(json!({ "ok": true, "action": "notify-away", "notifications": notifications }))
}

fn watch_once_from_db(conn: &Connection) -> Result<Vec<Value>> {
    let waiting = list_waiting_from_db(conn, None, 100)?;
    Ok(waiting
        .threads
        .into_iter()
        .map(|thread| {
            json!({
                "type": "thread_waiting",
                "threadId": thread.thread_id,
                "thread": {
                    "threadId": thread.thread_id,
                    "name": thread.name,
                    "cwd": thread.cwd,
                    "updatedAt": thread.updated_at,
                    "statusType": thread.status_type,
                    "statusFlags": thread.status_flags,
                    "lastPreview": thread.last_preview,
                    "pendingPrompt": {
                        "promptId": thread.prompt.prompt_id,
                        "promptKind": thread.prompt.kind,
                        "promptStatus": thread.prompt.status,
                        "question": thread.prompt.question
                    }
                }
            })
        })
        .collect())
}

struct CodexAppServerClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl CodexAppServerClient {
    fn connect() -> Result<Self> {
        let resolved = resolve_codex_binary()?;
        let mut child = Command::new(&resolved.path)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn {} app-server", resolved.path.display()))?;

        let stdin = child
            .stdin
            .take()
            .context("failed to open app-server stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to open app-server stdout")?;

        let mut client = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        };

        let _ = client.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "capabilities": {},
                "clientInfo": {
                    "name": "codex-hermes-bridge-rs",
                    "version": "0.1.0"
                }
            }),
        )?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let envelope = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_message(&envelope)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let envelope = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_message(&envelope)?;

        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line)?;
            if read == 0 {
                bail!("codex app-server closed stdout while waiting for {method}");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: Value = serde_json::from_str(trimmed)
                .with_context(|| format!("invalid JSON from app-server: {trimmed}"))?;
            match parsed.get("id") {
                Some(value) if value == &json!(id) => {
                    if let Some(error) = parsed.get("error") {
                        bail!("{method}: {}", error);
                    }
                    return Ok(parsed.get("result").cloned().unwrap_or(Value::Null));
                }
                _ => continue,
            }
        }
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        writeln!(self.stdin, "{}", serde_json::to_string(value)?)?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_waiting_prompt_from_status_flags() {
        let summary = json!({
            "id": "thr_reply",
            "name": null,
            "cwd": "/tmp/reply",
            "updatedAt": 123,
            "status": {
                "type": "active",
                "activeFlags": ["waitingOnUserInput"]
            }
        });
        let thread = json!({
            "id": "thr_reply",
            "cwd": "/tmp/reply",
            "status": {
                "type": "active",
                "activeFlags": ["waitingOnUserInput"]
            },
            "turns": [
                {
                    "status": "in_progress",
                    "items": [
                        {
                            "type": "agentMessage",
                            "phase": "final_answer",
                            "text": "Can you confirm the plan?"
                        }
                    ]
                }
            ]
        });

        let snapshot = normalize_thread_snapshot(&summary, &thread).expect("snapshot");
        let prompt = snapshot.pending_prompt.expect("pending prompt");
        assert_eq!(prompt.kind, "reply");
        assert_eq!(
            prompt.question.as_deref(),
            Some("Can you confirm the plan?")
        );
        assert_eq!(snapshot.last_turn_status.as_deref(), Some("in_progress"));
    }

    #[test]
    fn cli_accepts_ts_composed_follow_flags() {
        let new_cli = Cli::try_parse_from([
            "codex-hermes-bridge-rs",
            "new",
            "--message",
            "hello",
            "--follow",
            "--stream",
            "--duration",
            "500",
            "--poll-interval",
            "100",
            "--events",
            "follow_snapshot,item_completed",
        ]);
        assert!(new_cli.is_ok(), "new should accept TS follow flags: {new_cli:?}");

        let reply_cli = Cli::try_parse_from([
            "codex-hermes-bridge-rs",
            "reply",
            "thr_1",
            "--message",
            "ok",
            "--follow",
            "--duration",
            "500",
            "--events",
            "item_completed",
        ]);
        assert!(reply_cli.is_ok(), "reply should accept TS follow flags: {reply_cli:?}");

        let approve_cli = Cli::try_parse_from([
            "codex-hermes-bridge-rs",
            "approve",
            "thr_1",
            "approve",
            "--follow",
            "--stream",
        ]);
        assert!(approve_cli.is_ok(), "approve should accept positional decision and follow flags: {approve_cli:?}");

        let fork_cli = Cli::try_parse_from([
            "codex-hermes-bridge-rs",
            "fork",
            "thr_1",
            "--message",
            "try again",
            "--follow",
            "--experimental-realtime",
        ]);
        assert!(fork_cli.is_ok(), "fork should accept TS follow flags: {fork_cli:?}");
    }

    #[test]
    fn cli_accepts_ts_archive_filter_flags() {
        let cli = Cli::try_parse_from([
            "codex-hermes-bridge-rs",
            "archive",
            "--thread-id",
            "thr_1,thr_2",
            "--project",
            "bridge",
            "--status",
            "active",
            "--attention",
            "completed",
            "--limit",
            "5",
            "--dry-run",
        ]);
        assert!(cli.is_ok(), "archive should accept TS filter flags: {cli:?}");
    }

    #[test]
    fn state_schema_matches_ts_bridge_contract() {
        let conn = create_state_db_in_memory().expect("db");
        let tables = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .expect("prepare")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("tables");
        assert!(tables.contains(&"delivery_log".to_string()));
        assert!(tables.contains(&"notify_delivery_log".to_string()));

        let enabled = set_away_mode(&conn, true, 1234).expect("enable away");
        assert_eq!(enabled["away"], true);
        assert_eq!(
            get_setting_text(&conn, "away").expect("away setting").as_deref(),
            Some("true")
        );
        assert_eq!(
            get_setting_text(&conn, "away_mode").expect("legacy away setting"),
            None
        );
    }

    #[test]
    fn classifies_inbox_attention_states() {
        let approval = snapshot_fixture(
            "thr_approve",
            "/tmp/project-a",
            1000,
            "active",
            vec!["waitingOnApproval"],
            None,
        );
        let reply = snapshot_fixture(
            "thr_reply",
            "/tmp/project-b",
            900,
            "active",
            vec!["waitingOnUserInput"],
            None,
        );
        let completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-c",
            800,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        let active = snapshot_fixture("thr_active", "/tmp/project-d", 700, "active", vec![], None);

        let approval_item = classify_inbox_item(&approval, 2000);
        assert_eq!(approval_item.attention_reason, "pending_approval");
        assert_eq!(approval_item.waiting_on, "me");
        assert_eq!(approval_item.suggested_action, "approve");
        assert_eq!(approval_item.priority, "high");

        let reply_item = classify_inbox_item(&reply, 2000);
        assert_eq!(reply_item.attention_reason, "needs_reply");
        assert_eq!(reply_item.waiting_on, "me");
        assert_eq!(reply_item.suggested_action, "reply");

        let completed_item = classify_inbox_item(&completed, 2000);
        assert_eq!(completed_item.attention_reason, "completed");
        assert_eq!(completed_item.waiting_on, "none");
        assert_eq!(completed_item.suggested_action, "archive");

        let active_item = classify_inbox_item(&active, 2000);
        assert_eq!(active_item.attention_reason, "active");
        assert_eq!(active_item.waiting_on, "codex");
        assert_eq!(active_item.suggested_action, "inspect");
    }

    #[test]
    fn persists_snapshots_and_actions_in_sqlite_state() {
        let conn = create_state_db_in_memory().expect("db");
        let waiting = snapshot_fixture(
            "thr_wait",
            "/tmp/project-wait",
            1200,
            "active",
            vec!["waitingOnUserInput"],
            Some("in_progress"),
        );
        let done = snapshot_fixture(
            "thr_done",
            "/tmp/project-done",
            1300,
            "notLoaded",
            vec![],
            Some("completed"),
        );

        upsert_thread_snapshot(&conn, &waiting, 2000).expect("upsert waiting");
        upsert_thread_snapshot(&conn, &done, 2000).expect("upsert done");
        record_action(
            &conn,
            "thr_wait",
            "reply",
            json!({"message": "On it"}),
            2100,
        )
        .expect("record action");

        let waiting_rows = list_waiting_from_db(&conn, None, 10).expect("waiting rows");
        assert_eq!(waiting_rows.threads.len(), 1);
        assert_eq!(waiting_rows.threads[0].thread_id, "thr_wait");
        assert_eq!(waiting_rows.threads[0].prompt.kind, "reply");

        let history = get_thread_history(&conn, "thr_wait", 10).expect("history");
        assert_eq!(history[0].action_type, "reply");

        let changes = list_changes_since_last_check(&conn, 2200).expect("changes");
        assert_eq!(changes.summary.total_count, 2);

        let done_rows = list_done_since_last_check(&conn, 2300).expect("done rows");
        assert_eq!(done_rows.threads.len(), 1);
        assert_eq!(done_rows.threads[0]["threadId"], "thr_done");
    }

    #[test]
    fn sqlite_backed_inbox_matches_cached_attention() {
        let conn = create_state_db_in_memory().expect("db");
        let approval = snapshot_fixture(
            "thr_approve",
            "/tmp/project-a",
            1000,
            "active",
            vec!["waitingOnApproval"],
            Some("in_progress"),
        );
        let completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-b",
            1100,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        upsert_thread_snapshot(&conn, &approval, 2000).expect("upsert approval");
        upsert_thread_snapshot(&conn, &completed, 2000).expect("upsert completed");

        let inbox = list_inbox_from_db(&conn, 3000, None, None, None, None, 10).expect("inbox");
        assert_eq!(inbox.summary.total, 2);
        assert_eq!(inbox.summary.needs_attention, 1);
        assert_eq!(inbox.items[0].attention_reason, "pending_approval");
        assert_eq!(inbox.items[0].suggested_action, "approve");
    }

    #[test]
    fn new_and_fork_dry_run_shapes_are_stable() {
        let new_result = start_new_thread_dry_run(Some("/tmp/project"), Some("hello"));
        assert_eq!(new_result["action"], "new");
        assert_eq!(new_result["dryRun"], true);
        assert_eq!(new_result["cwd"], "/tmp/project");

        let fork_result = fork_thread_dry_run("thr_source", Some("follow up"));
        assert_eq!(fork_result["action"], "fork");
        assert_eq!(fork_result["dryRun"], true);
        assert_eq!(fork_result["fromThreadId"], "thr_source");
    }

    #[test]
    fn archive_requires_yes_for_filter_selection_and_supports_dry_run() {
        let conn = create_state_db_in_memory().expect("db");
        let completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-b",
            1100,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        upsert_thread_snapshot(&conn, &completed, 2000).expect("upsert completed");

        let err = archive_from_db(&conn, None, Some("completed"), false, false, 3000)
            .expect_err("should require yes");
        assert!(err.to_string().contains("Refusing bulk archive"));

        let dry_run = archive_from_db(&conn, None, Some("completed"), true, false, 3000)
            .expect("dry run archive");
        assert_eq!(dry_run["dryRun"], true);
        assert_eq!(dry_run["results"][0]["status"], "would_archive");
    }

    #[test]
    fn notify_away_dedupes_within_same_session() {
        let conn = create_state_db_in_memory().expect("db");
        let waiting = snapshot_fixture(
            "thr_wait",
            "/tmp/project-wait",
            1500,
            "active",
            vec!["waitingOnUserInput"],
            Some("in_progress"),
        );
        upsert_thread_snapshot(&conn, &waiting, 2000).expect("upsert waiting");
        set_setting_text(&conn, "away_mode", "true").expect("set away mode");
        set_setting_text(&conn, "away_session_id", "session-1").expect("set away session");
        set_setting(&conn, "away_started_at", 1000).expect("set away started at");

        let first = build_notify_away_from_db(&conn, 2500, true).expect("first notify");
        assert_eq!(first["action"], "notify-away");
        assert_eq!(first["notifications"].as_array().unwrap().len(), 1);
        assert!(first["notifications"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Tell me how you want me to reply"));

        let second = build_notify_away_from_db(&conn, 2600, true).expect("second notify");
        assert_eq!(second["notifications"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn watch_once_emits_cached_waiting_event_shape() {
        let conn = create_state_db_in_memory().expect("db");
        let waiting = snapshot_fixture(
            "thr_wait",
            "/tmp/project-wait",
            1500,
            "active",
            vec!["waitingOnUserInput"],
            Some("in_progress"),
        );
        upsert_thread_snapshot(&conn, &waiting, 2000).expect("upsert waiting");

        let events = watch_once_from_db(&conn).expect("watch once");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "thread_waiting");
        assert_eq!(events[0]["threadId"], "thr_wait");
        assert_eq!(events[0]["thread"]["pendingPrompt"]["promptKind"], "reply");
    }

    #[test]
    fn shared_notification_renderer_preserves_context_first() {
        let rendered = render_notification_message(
            "thread_completed",
            "Sample thread",
            "project-a",
            Some("/tmp/project-a"),
            Some("Finished work"),
            None,
            Some("Last action: replied"),
            Some("Tell me if you want me to follow up"),
        );
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "🔔 Codex update");
        assert!(rendered.contains("• Thread: Sample thread"));
    }

    #[test]
    fn away_mode_round_trip_matches_ts_shape() {
        let conn = create_state_db_in_memory().expect("db");

        let initial = get_away_mode(&conn).expect("initial away");
        assert_eq!(initial["away"], false);
        assert_eq!(initial["awayStartedAt"], Value::Null);

        let enabled = set_away_mode(&conn, true, 1234).expect("enable away");
        assert_eq!(enabled["away"], true);
        assert_eq!(enabled["awayStartedAt"], 1234);
        assert_eq!(enabled["awaySessionId"], "1234");

        let status = get_away_mode(&conn).expect("status away");
        assert_eq!(status["away"], true);
        assert_eq!(status["awayStartedAt"], 1234);
        assert_eq!(status["awaySessionId"], "1234");

        let disabled = set_away_mode(&conn, false, 1500).expect("disable away");
        assert_eq!(disabled["away"], false);
        assert_eq!(disabled["awayStartedAt"], Value::Null);
        assert_eq!(disabled["awaySessionId"], Value::Null);
    }

    #[test]
    fn unarchive_dry_run_and_live_record_action() {
        let conn = create_state_db_in_memory().expect("db");

        let dry_run = unarchive_thread_result(&conn, "thr_archived", true, 1000, None).expect("dry run");
        assert_eq!(dry_run["action"], "unarchive");
        assert_eq!(dry_run["results"][0]["status"], "would_unarchive");

        let live = unarchive_thread_result(
            &conn,
            "thr_archived",
            false,
            1100,
            Some(json!({"threadId": "thr_archived", "ok": true})),
        )
        .expect("live");
        assert_eq!(live["results"][0]["status"], "unarchived");

        let history = get_thread_history(&conn, "thr_archived", 10).expect("history");
        assert_eq!(history[0].action_type, "unarchive");
    }

    #[test]
    fn status_audit_derives_attention_and_action() {
        let conn = create_state_db_in_memory().expect("db");
        let approval = snapshot_fixture(
            "thr_approve",
            "/tmp/project-a",
            1000,
            "active",
            vec!["waitingOnApproval"],
            Some("in_progress"),
        );
        upsert_thread_snapshot(&conn, &approval, 2000).expect("upsert approval");

        let audit = get_status_audit(&conn, Some("thr_approve")).expect("audit");
        assert_eq!(audit["audits"][0]["threadId"], "thr_approve");
        assert_eq!(audit["audits"][0]["derived"]["attentionReason"], "pending_approval");
        assert_eq!(audit["audits"][0]["derived"]["suggestedAction"], "approve");
        assert_eq!(audit["audits"][0]["derived"]["waitingOn"], "me");
    }

    #[test]
    fn doctor_realtime_shape_handles_missing_realtime_support() {
        let payload = doctor_realtime_result(
            "thr_1",
            false,
            Ok(json!({"thread": {"id": "thr_1"}})),
            Ok(json!({"thread": {"id": "thr_1"}})),
            None,
            None,
        );
        assert_eq!(payload["action"], "doctor-realtime");
        assert_eq!(payload["threadId"], "thr_1");
        assert_eq!(payload["checks"]["realtimeStart"], Value::Null);
        assert_eq!(payload["checks"]["realtimeStop"], Value::Null);
    }

    #[test]
    fn follow_result_emits_started_and_snapshot_events() {
        let thread = json!({"id": "thr_follow", "name": "Follow me"});
        let events = build_follow_events("thr_follow", 1000, 500, false, Some(thread.clone()), None, vec![])
            .expect("follow events");
        assert_eq!(events[0]["type"], "follow_started");
        assert_eq!(events[1]["type"], "follow_snapshot");
        assert_eq!(events[1]["thread"], thread);
    }

    #[test]
    fn brief_reads_fake_codex_corpus() {
        let root = std::env::temp_dir().join(format!("codex-brief-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".codex/sessions/2026/04/13")).expect("mkdir sessions");
        std::fs::write(
            root.join(".codex/session_index.jsonl"),
            "{\"id\":\"sess-1\",\"thread_name\":\"Stripe webhook issue\",\"updated_at\":\"2026-04-13T12:00:00Z\"}\n",
        )
        .expect("write index");
        std::fs::write(
            root.join(".codex/sessions/2026/04/13/rollout-2026-04-13T12-00-00-sess-1.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"sess-1\",\"cwd\":\"/tmp/project\",\"timestamp\":\"2026-04-13T12:00:00Z\"}}\n{\"timestamp\":\"2026-04-13T12:01:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Please debug the Stripe webhook failure\"}]}}\n{\"timestamp\":\"2026-04-13T12:02:00Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"We should check webhook signing and retry behavior first\"}]}}\n",
        )
        .expect("write session");

        let previous_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", &root);
        let result = build_brief("Stripe webhook", 2).expect("brief");
        if let Some(home) = previous_home {
            std::env::set_var("HOME", home);
        }

        assert_eq!(result["query"], "Stripe webhook");
        assert_eq!(result["sessions"][0]["session_id"], "sess-1");
        assert_eq!(result["sessions"][0]["thread_name"], "Stripe webhook issue");
    }

    #[test]
    fn watch_filter_keeps_only_requested_event_types() {
        let filtered = filter_watch_events(
            vec![
                json!({"type": "thread_waiting", "threadId": "thr_1"}),
                json!({"type": "item_completed", "threadId": "thr_1"}),
            ],
            Some(&std::collections::BTreeSet::from(["item_completed".to_string()])),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["type"], "item_completed");
    }

    #[test]
    fn watch_exec_hook_writes_event_to_stdout_consumer() {
        let temp = std::env::temp_dir().join(format!("codex-watch-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).expect("mkdir temp");
        let hook_output = temp.join("hook.json");
        let command = format!("python3 -c \"import sys,pathlib; pathlib.Path(r'{}').write_text(sys.stdin.read())\"", hook_output.display());
        run_exec_hook(&command, &json!({"type": "thread_waiting", "threadId": "thr_1"})).expect("exec hook");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let payload = std::fs::read_to_string(hook_output).expect("hook output");
        assert!(payload.contains("thread_waiting"));
    }

    fn snapshot_fixture(
        thread_id: &str,
        cwd: &str,
        updated_at: u64,
        status_type: &str,
        status_flags: Vec<&str>,
        last_turn_status: Option<&str>,
    ) -> BridgeThreadSnapshot {
        let status_flags_vec = status_flags
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        BridgeThreadSnapshot {
            thread_id: thread_id.to_string(),
            name: None,
            cwd: Some(cwd.to_string()),
            updated_at: Some(updated_at),
            status_type: status_type.to_string(),
            status_flags: status_flags_vec.clone(),
            last_turn_status: last_turn_status.map(|s| s.to_string()),
            last_preview: Some(format!("preview for {thread_id}")),
            pending_prompt: derive_pending_prompt(
                thread_id,
                &status_flags_vec,
                Some(format!("preview for {thread_id}")),
            ),
        }
    }
}
