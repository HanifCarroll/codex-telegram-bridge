use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use notify::Watcher as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn fixture_path() -> Option<PathBuf> {
    env::var("CODEX_HERMES_BRIDGE_FIXTURE")
        .ok()
        .map(PathBuf::from)
}

fn fixture_actions_log_path() -> Option<PathBuf> {
    env::var("CODEX_HERMES_BRIDGE_ACTIONS_LOG")
        .ok()
        .map(PathBuf::from)
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
#[command(name = "codex-hermes-bridge")]
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
        #[arg(long, default_value_t = true)]
        completed: bool,
        #[arg(long, default_value_t = false)]
        mark_delivered: bool,
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
    classified: Value,
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
    #[serde(rename = "lastSeenAt")]
    last_seen_at: Option<u64>,
    #[serde(rename = "ageSeconds")]
    age_seconds: Option<u64>,
    #[serde(rename = "statusType")]
    status_type: String,
    #[serde(rename = "statusFlags")]
    status_flags: Vec<String>,
    #[serde(rename = "lastPreview")]
    last_preview: Option<String>,
    #[serde(skip)]
    delivery_preview: Option<String>,
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
                classified: classify_app_server_error_message(&format!("{error:#}")),
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
                let mut client = CodexAppServerClient::connect_with_options(experimental_realtime)?;
                let with_turns = client.request(
                    "thread/read",
                    json!({ "threadId": thread_id, "includeTurns": true }),
                );
                let without_turns = client.request(
                    "thread/read",
                    json!({ "threadId": thread_id, "includeTurns": false }),
                );
                let realtime_start = if experimental_realtime {
                    Some(client.request("thread/realtime/start", json!({ "threadId": thread_id })))
                } else {
                    None
                };
                let realtime_stop = if experimental_realtime {
                    Some(client.request("thread/realtime/stop", json!({ "threadId": thread_id })))
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
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if load_fixture_into_db(&conn, now)? {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "threads": list_cached_threads(&conn, limit)?
                    }))?
                );
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let result = sync_state_from_live(&mut client, &conn, now, limit, false)?;
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "threads": result["threads"].clone()
                    }))?
                );
            }
        }
        Commands::Follow {
            thread_id,
            message,
            duration,
            poll_interval,
            events,
            experimental_realtime,
        } => {
            let event_filter = parse_event_filter(events.as_deref());
            let mut client = CodexAppServerClient::connect_with_options(experimental_realtime)?;
            let events = collect_follow_events(
                &mut client,
                &thread_id,
                message.as_deref(),
                duration,
                poll_interval,
                experimental_realtime,
                event_filter.as_ref(),
            )?;
            for event in &events {
                println!("{}", serde_json::to_string(event)?);
            }
            println!(
                "{}",
                serde_json::to_string(&follow_result_summary(
                    &thread_id,
                    duration,
                    experimental_realtime,
                    &events,
                    false,
                ))?
            );
        }
        Commands::StatusAudit { thread_id } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !load_fixture_into_db(&conn, now)? {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, 50, false)?;
            }
            println!(
                "{}",
                serde_json::to_string(&get_status_audit(&conn, thread_id.as_deref())?)?
            );
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
            println!(
                "{}",
                serde_json::to_string(&unarchive_thread_result(
                    &conn,
                    &thread_id,
                    dry_run,
                    now,
                    live_result
                )?)?
            );
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
                sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
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
                sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
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
            let filter = parse_event_filter(events.as_deref());
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if once {
                let now = now_millis()?;
                let sync_result = if let Some(threads) = fixture_threads()? {
                    let mut snapshots = Vec::new();
                    for thread in threads {
                        let snapshot = normalize_thread_snapshot(&thread, &thread)?;
                        snapshots.push(snapshot);
                    }
                    reconcile_thread_snapshots(&conn, now, snapshots, true)?
                } else {
                    let mut client = CodexAppServerClient::connect()?;
                    let filtered = match sync_state_from_live(&mut client, &conn, now, 50, true) {
                        Ok(sync_result) => {
                            let notifications = client.drain_notifications();
                            watch_events_from_sync_result(
                                &sync_result,
                                notifications,
                                filter.as_ref(),
                            )
                        }
                        Err(error) => filter_watch_events(
                            vec![watch_thread_error_event(&error)],
                            filter.as_ref(),
                        ),
                    };
                    for event in &filtered {
                        if let Some(command) = exec.as_deref() {
                            run_exec_hook(command, event)?;
                        }
                    }
                    println!("{}", serde_json::to_string(&json!({ "events": filtered }))?);
                    return Ok(());
                };
                let filtered = watch_events_from_sync_result(&sync_result, vec![], filter.as_ref());
                for event in &filtered {
                    if let Some(command) = exec.as_deref() {
                        run_exec_hook(command, event)?;
                    }
                }
                println!("{}", serde_json::to_string(&json!({ "events": filtered }))?);
            } else {
                println!(
                    "{}",
                    serde_json::to_string(
                        &json!({ "type": "watch_started", "away": get_away_mode(&conn)?["away"] })
                    )?
                );
                let mut last = String::new();
                let watch_rx = start_codex_watch_receiver().ok();
                loop {
                    let now = now_millis()?;
                    let mut client = CodexAppServerClient::connect()?;
                    let filtered = match sync_state_from_live(&mut client, &conn, now, 50, true) {
                        Ok(sync_result) => watch_events_from_sync_result(
                            &sync_result,
                            client.drain_notifications(),
                            filter.as_ref(),
                        ),
                        Err(error) => filter_watch_events(
                            vec![watch_thread_error_event(&error)],
                            filter.as_ref(),
                        ),
                    };
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
                    if let Some(rx) = watch_rx.as_ref() {
                        rx.recv_timeout(std::time::Duration::from_millis(1500));
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                    }
                }
            }
        }
        Commands::NotifyAway {
            completed,
            mark_delivered,
        } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !load_fixture_into_db(&conn, now)? {
                let mut client = CodexAppServerClient::connect_with_options(true)?;
                sync_state_from_live(&mut client, &conn, now, 50, true)?;
            }
            let result = build_notify_away_from_db(&conn, now, completed, mark_delivered)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Sync { limit } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let result = if let Some(threads) = fixture_threads()? {
                let mut snapshots = Vec::new();
                for thread in threads {
                    snapshots.push(normalize_thread_snapshot(&thread, &thread)?);
                }
                reconcile_thread_snapshots(&conn, now, snapshots, false)?
            } else {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, limit, false)?
            };
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Changes => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !load_fixture_into_db(&conn, now)? {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, 50, false)?;
            }
            let result = list_changes_since_last_check(&conn, now)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "summary": {
                        "count": result.summary.total_count,
                        "lastCheckedAt": result.summary.last_checked_at,
                        "currentCursor": result.summary.current_cursor
                    },
                    "threads": result.threads
                }))?
            );
        }
        Commands::Done => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !load_fixture_into_db(&conn, now)? {
                let mut client = CodexAppServerClient::connect()?;
                sync_state_from_live(&mut client, &conn, now, 50, false)?;
            }
            let result = list_done_since_last_check(&conn, now)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "summary": {
                        "count": result.summary.total_count,
                        "lastCheckedAt": result.summary.last_checked_at,
                        "currentCursor": result.summary.current_cursor
                    },
                    "threads": result.threads
                }))?
            );
        }
        Commands::History { thread_id, limit } => {
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let actions = get_thread_history(&conn, &thread_id, limit)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "threadId": thread_id,
                    "actions": actions.iter().map(|action| json!({
                        "actionType": action.action_type,
                        "createdAt": action.created_at,
                        "payload": action.payload
                    })).collect::<Vec<_>>()
                }))?
            );
        }
        Commands::New {
            cwd,
            message,
            dry_run,
            follow,
            stream,
            duration,
            poll_interval,
            events,
            experimental_realtime,
            prompt,
        } => {
            let message = normalized_message(message.as_deref()).or_else(|| {
                let joined = prompt.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            if dry_run {
                println!(
                    "{}",
                    serde_json::to_string(&start_new_thread_dry_run(
                        cwd.as_deref(),
                        message.as_deref()
                    ))?
                );
            } else {
                let mut client = CodexAppServerClient::connect_with_options(experimental_realtime)?;
                let created = client.request(
                    "thread/start",
                    thread_start_params(cwd.as_deref(), message.as_deref()),
                )?;
                let thread_id = thread_id_from_response(&created);
                let started = match (thread_id.as_deref(), message.as_deref()) {
                    (Some(thread_id), Some(message)) => Some(client.request(
                        "turn/start",
                        turn_start_params(thread_id, cwd.as_deref(), message),
                    )?),
                    _ => None,
                };
                let result =
                    new_thread_live_result(cwd.as_deref(), message.as_deref(), created, started);
                let result = if follow {
                    let db_path = state_db_path()?;
                    let conn = create_state_db(&db_path)?;
                    let filter = parse_event_filter(events.as_deref());
                    if let Some(thread_id) = result.get("threadId").and_then(Value::as_str) {
                        attach_follow_result(
                            result.clone(),
                            &mut client,
                            &conn,
                            FollowRun {
                                thread_id,
                                duration_ms: duration,
                                poll_interval_ms: poll_interval,
                                experimental_realtime,
                                event_filter: filter.as_ref(),
                                stream,
                            },
                        )?
                    } else {
                        result
                    }
                } else {
                    result
                };
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Commands::Fork {
            thread_id,
            message,
            dry_run,
            follow,
            stream,
            duration,
            poll_interval,
            events,
            experimental_realtime,
            prompt,
        } => {
            let message = normalized_message(message.as_deref()).or_else(|| {
                let joined = prompt.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            if dry_run {
                println!(
                    "{}",
                    serde_json::to_string(&fork_thread_dry_run(&thread_id, message.as_deref()))?
                );
            } else {
                let mut client = CodexAppServerClient::connect_with_options(experimental_realtime)?;
                let forked = client.request("thread/fork", json!({ "threadId": thread_id }))?;
                let new_thread_id = thread_id_from_response(&forked);
                let forked_cwd = thread_cwd_from_response(&forked, None);
                let started = match (new_thread_id.as_deref(), message.as_deref()) {
                    (Some(new_thread_id), Some(message)) if !message.trim().is_empty() => {
                        Some(client.request(
                            "turn/start",
                            turn_start_params(new_thread_id, forked_cwd.as_deref(), message),
                        )?)
                    }
                    _ => None,
                };
                let result =
                    fork_thread_live_result(&thread_id, message.as_deref(), forked, started);
                let result = if follow {
                    let db_path = state_db_path()?;
                    let conn = create_state_db(&db_path)?;
                    let filter = parse_event_filter(events.as_deref());
                    if let Some(thread_id) = result.get("threadId").and_then(Value::as_str) {
                        attach_follow_result(
                            result.clone(),
                            &mut client,
                            &conn,
                            FollowRun {
                                thread_id,
                                duration_ms: duration,
                                poll_interval_ms: poll_interval,
                                experimental_realtime,
                                event_filter: filter.as_ref(),
                                stream,
                            },
                        )?
                    } else {
                        result
                    }
                } else {
                    result
                };
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Commands::Archive {
            thread_id_option,
            thread_ids,
            project,
            status,
            attention,
            limit,
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
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let fixture_loaded = load_fixture_into_db(&conn, now)?;
            if !dry_run && targets.is_empty() && !yes {
                bail!("Refusing bulk archive without --yes or --dry-run");
            }
            let mut client = if dry_run {
                None
            } else {
                Some(CodexAppServerClient::connect()?)
            };
            if !dry_run && targets.is_empty() && !fixture_loaded {
                if let Some(client) = client.as_mut() {
                    sync_state_from_live(client, &conn, now, 50, false)?;
                }
            }
            let selection = resolve_archive_targets(
                &conn,
                &targets,
                project.as_deref(),
                status.as_deref(),
                attention.as_deref(),
                limit,
                now,
            )?;
            if !dry_run && selection.using_filter_selection && !yes {
                bail!("Refusing bulk archive without --yes or --dry-run");
            }
            if dry_run {
                let results = selection
                    .targets
                    .into_iter()
                    .map(|thread_id| json!({ "threadId": thread_id, "status": "would_archive" }))
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string(&archive_result(true, results))?);
            } else {
                let mut results = Vec::new();
                for target in selection.targets {
                    let result = client
                        .as_mut()
                        .context("archive client missing")?
                        .request("thread/archive", json!({ "threadId": target }))?;
                    record_action(
                        &conn,
                        &target,
                        "archive",
                        json!({ "result": result, "archivedAt": now }),
                        now,
                    )?;
                    results.push(json!({
                        "threadId": target,
                        "status": "archived",
                        "result": result
                    }));
                }
                println!(
                    "{}",
                    serde_json::to_string(&archive_result(false, results))?
                );
            }
        }
        Commands::Show { thread_id } => {
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if let Some(result) = fixture_show_thread(&thread_id)? {
                println!(
                    "{}",
                    serde_json::to_string(&build_show_thread_result(
                        Some(&conn),
                        &thread_id,
                        result,
                    )?)?
                );
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let result = client.request(
                    "thread/read",
                    json!({
                        "threadId": thread_id,
                        "includeTurns": true
                    }),
                )?;
                println!(
                    "{}",
                    serde_json::to_string(&build_show_thread_result(
                        Some(&conn),
                        &thread_id,
                        result,
                    )?)?
                );
            }
        }
        Commands::Reply {
            thread_id,
            message,
            dry_run,
            follow,
            stream,
            duration,
            poll_interval,
            events,
            experimental_realtime,
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
                let mut client = CodexAppServerClient::connect_with_options(experimental_realtime)?;
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
                let result = json!({
                    "ok": true,
                    "action": "reply",
                    "threadId": thread_id,
                    "message": message,
                    "sentAt": sent_at,
                    "resumed": resumed,
                    "started": started
                });
                let result = if follow {
                    let filter = parse_event_filter(events.as_deref());
                    attach_follow_result(
                        result,
                        &mut client,
                        &conn,
                        FollowRun {
                            thread_id: &thread_id,
                            duration_ms: duration,
                            poll_interval_ms: poll_interval,
                            experimental_realtime,
                            event_filter: filter.as_ref(),
                            stream,
                        },
                    )?
                } else {
                    result
                };
                println!("{}", serde_json::to_string(&result)?);
            }
        }
        Commands::Approve {
            thread_id,
            decision,
            dry_run,
            follow,
            stream,
            duration,
            poll_interval,
            events,
            experimental_realtime: _experimental_realtime,
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
                let result = json!({
                    "ok": true,
                    "action": "approve",
                    "threadId": thread_id,
                    "decision": normalized,
                    "sentText": sent_text,
                    "sentAt": sent_at,
                    "resumed": resumed,
                    "started": started
                });
                let result = if follow {
                    let filter = parse_event_filter(events.as_deref());
                    attach_follow_result(
                        result,
                        &mut client,
                        &conn,
                        FollowRun {
                            thread_id: &thread_id,
                            duration_ms: duration,
                            poll_interval_ms: poll_interval,
                            experimental_realtime: false,
                            event_filter: filter.as_ref(),
                            stream,
                        },
                    )?
                } else {
                    result
                };
                println!("{}", serde_json::to_string(&result)?);
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

fn preserve_message_body(input: Option<&str>, limit: usize) -> Option<String> {
    let mut lines = Vec::new();
    let mut blank_count = 0;
    let normalized_newlines = input?.replace("\r\n", "\n");
    for line in normalized_newlines.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                lines.push(String::new());
            }
            continue;
        }
        blank_count = 0;
        lines.push(trimmed.to_string());
    }
    let normalized = lines.join("\n").trim().to_string();
    if normalized.is_empty() {
        return None;
    }
    if normalized.chars().count() <= limit {
        return Some(normalized);
    }
    let kept = normalized
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    Some(format!("{}…", kept.trim_end()))
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

fn extract_item_text(item: &Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(Value::as_str).map(str::trim) {
        if !text.is_empty() {
            return Some(text.to_string());
        }
    }

    let text = item
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn find_last_message_by_type(thread: &Value, item_type: &str) -> Option<String> {
    let turns = thread.get("turns").and_then(Value::as_array)?;
    for turn in turns.iter().rev() {
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items.iter().rev() {
            if item.get("type").and_then(Value::as_str) == Some(item_type) {
                if let Some(text) = extract_item_text(item) {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn latest_question(thread: &Value) -> Option<String> {
    let last_user_message = find_last_message_by_type(thread, "userMessage")?;
    last_user_message.contains('?').then_some(last_user_message)
}

fn show_pending_prompt(status_flags: &[String]) -> Option<Value> {
    if status_flags.iter().any(|flag| flag == "waitingOnApproval") {
        return Some(json!({
            "kind": "approval",
            "promptStatus": "Needs approval"
        }));
    }
    if status_flags
        .iter()
        .any(|flag| flag == "waitingOnUserInput" || flag == "waitingOnInput")
    {
        return Some(json!({
            "kind": "reply",
            "promptStatus": "Needs input"
        }));
    }
    None
}

fn build_delta_summary(
    pending_prompt: Option<&Value>,
    last_turn_status: Option<&str>,
    last_agent_message: Option<&str>,
) -> Option<String> {
    let agent_suffix = last_agent_message
        .filter(|value| !value.trim().is_empty())
        .map(|message| format!(" · Last agent update: {message}"))
        .unwrap_or_default();

    if let Some(prompt) = pending_prompt {
        let prompt_status = prompt
            .get("promptStatus")
            .and_then(Value::as_str)
            .unwrap_or("Needs input");
        return Some(format!("{prompt_status}{agent_suffix}"));
    }

    if let Some(status) = last_turn_status {
        return Some(format!("Last turn status: {status}{agent_suffix}"));
    }

    last_agent_message.map(str::to_string)
}

fn build_unresolved_summary(
    pending_prompt: Option<&Value>,
    latest_question: Option<&str>,
    last_user_message: Option<&str>,
) -> Option<String> {
    if pending_prompt
        .and_then(|prompt| prompt.get("kind"))
        .and_then(Value::as_str)
        == Some("approval")
    {
        return Some("Approval is still pending.".to_string());
    }

    if let Some(question) = latest_question {
        return Some(format!("Latest unresolved question: {question}"));
    }

    if pending_prompt
        .and_then(|prompt| prompt.get("kind"))
        .and_then(Value::as_str)
        == Some("reply")
    {
        if let Some(message) = last_user_message {
            return Some(format!("Reply still needed: {message}"));
        }
    }

    None
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
        _ if snapshot.last_turn_status.as_deref() == Some("interrupted")
            && snapshot
                .last_preview
                .as_deref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false) =>
        {
            ("updated", "last_turn_interrupted")
        }
        _ => ("active", "fallback_active"),
    }
}

fn classify_waiting_on(reason: &str) -> &'static str {
    match reason {
        "pending_approval" | "needs_reply" => "me",
        "active" => "codex",
        "updated" => "none",
        _ => "none",
    }
}

fn classify_suggested_action(reason: &str) -> &'static str {
    match reason {
        "pending_approval" => "approve",
        "needs_reply" => "reply",
        "completed" => "archive",
        "updated" => "inspect",
        _ => "inspect",
    }
}

fn classify_priority(reason: &str) -> &'static str {
    match reason {
        "pending_approval" => "high",
        "needs_reply" | "active" | "updated" => "medium",
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
        last_seen_at: None,
        age_seconds: snapshot
            .updated_at
            .map(|updated| now.saturating_sub(updated)),
        status_type: snapshot.status_type.clone(),
        status_flags: snapshot.status_flags.clone(),
        last_preview: compact_text_preview(snapshot.last_preview.clone(), 220),
        delivery_preview: snapshot.last_preview.clone(),
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
        CREATE TABLE IF NOT EXISTS thread_events (
          event_key TEXT PRIMARY KEY,
          thread_id TEXT NOT NULL,
          event_type TEXT NOT NULL,
          observed_at INTEGER NOT NULL,
          payload_json TEXT NOT NULL
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
    ensure_column(conn, "threads_cache", "last_seen_at", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "threads_cache", "last_turn_status", "TEXT")?;
    ensure_column(conn, "threads_cache", "last_preview", "TEXT")?;
    Ok(())
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let columns = table_columns(conn, table)?;
    if !columns.iter().any(|current| current == column) {
        conn.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
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

fn upsert_thread_snapshot(
    conn: &Connection,
    snapshot: &BridgeThreadSnapshot,
    now: u64,
) -> Result<()> {
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
            updated_at = CASE
                WHEN threads_cache.updated_at IS NULL THEN excluded.updated_at
                WHEN excluded.updated_at IS NULL THEN threads_cache.updated_at
                WHEN excluded.updated_at > threads_cache.updated_at THEN excluded.updated_at
                ELSE threads_cache.updated_at
            END,
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

fn recent_actions_json(conn: &Connection, thread_id: &str, limit: u64) -> Result<Vec<Value>> {
    Ok(get_thread_history(conn, thread_id, limit)?
        .into_iter()
        .map(|action| {
            json!({
                "actionType": action.action_type,
                "createdAt": action.created_at,
                "payload": action.payload
            })
        })
        .collect())
}

fn thread_snapshot_json(snapshot: &BridgeThreadSnapshot) -> Value {
    json!({
        "threadId": snapshot.thread_id,
        "name": snapshot.name,
        "cwd": snapshot.cwd,
        "updatedAt": snapshot.updated_at,
        "statusType": snapshot.status_type,
        "statusFlags": snapshot.status_flags,
        "lastTurnStatus": snapshot.last_turn_status,
        "lastPreview": snapshot.last_preview,
        "pendingPrompt": snapshot.pending_prompt.as_ref().map(|prompt| json!({
            "promptId": prompt.prompt_id,
            "promptKind": prompt.kind,
            "promptStatus": prompt.status,
            "question": prompt.question
        }))
    })
}

fn should_emit_for_away_window(away_started_at: Option<u64>, updated_at: Option<u64>) -> bool {
    match away_started_at {
        None => true,
        Some(started_at) => updated_at.map(|value| value >= started_at).unwrap_or(false),
    }
}

fn record_delivery(
    conn: &Connection,
    event_key: &str,
    thread_id: &str,
    event_type: &str,
    delivered_at: u64,
) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO delivery_log(event_key, thread_id, event_type, delivered_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![event_key, thread_id, event_type, to_sql_i64(delivered_at)?],
    )?;
    Ok(changed > 0)
}

fn record_thread_event(
    conn: &Connection,
    event_key: &str,
    thread_id: &str,
    event_type: &str,
    observed_at: u64,
    payload: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO thread_events(event_key, thread_id, event_type, observed_at, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            event_key,
            thread_id,
            event_type,
            to_sql_i64(observed_at)?,
            serde_json::to_string(payload)?
        ],
    )?;
    Ok(())
}

fn reconcile_thread_snapshots(
    conn: &Connection,
    now: u64,
    snapshots: Vec<BridgeThreadSnapshot>,
    record_deliveries: bool,
) -> Result<Value> {
    let away = get_setting_text(conn, "away")?.unwrap_or_default() == "true";
    let away_started_at = get_setting_number(conn, "away_started_at")?;
    let mut events = Vec::new();
    let mut threads = Vec::new();

    for snapshot in &snapshots {
        upsert_thread_snapshot(conn, snapshot, now)?;
        threads.push(thread_snapshot_json(snapshot));

        if !away || !should_emit_for_away_window(away_started_at, snapshot.updated_at) {
            continue;
        }

        if let Some(prompt) = &snapshot.pending_prompt {
            let event_key = format!(
                "thread_waiting:{}:{}:{}",
                snapshot.thread_id,
                prompt.prompt_id,
                snapshot
                    .updated_at
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "none".to_string())
            );
            let should_emit = !record_deliveries
                || record_delivery(conn, &event_key, &snapshot.thread_id, "thread_waiting", now)?;
            if should_emit {
                let event = json!({
                    "type": "thread_waiting",
                    "threadId": snapshot.thread_id,
                    "promptKind": prompt.kind,
                    "updatedAt": snapshot.updated_at,
                    "lastPreview": snapshot.last_preview,
                });
                record_thread_event(
                    conn,
                    &event_key,
                    &snapshot.thread_id,
                    "thread_waiting",
                    now,
                    &event,
                )?;
                events.push(event);
            }
        }

        if snapshot.pending_prompt.is_none()
            && snapshot.last_turn_status.as_deref() == Some("completed")
        {
            let event_key = format!(
                "thread_completed:{}:{}",
                snapshot.thread_id,
                snapshot
                    .updated_at
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "none".to_string())
            );
            let should_emit = !record_deliveries
                || record_delivery(
                    conn,
                    &event_key,
                    &snapshot.thread_id,
                    "thread_completed",
                    now,
                )?;
            if should_emit {
                let event = json!({
                    "type": "thread_completed",
                    "threadId": snapshot.thread_id,
                    "updatedAt": snapshot.updated_at,
                    "lastPreview": snapshot.last_preview,
                });
                record_thread_event(
                    conn,
                    &event_key,
                    &snapshot.thread_id,
                    "thread_completed",
                    now,
                    &event,
                )?;
                events.push(event);
            }
        }
    }

    Ok(json!({
        "synced": snapshots.len(),
        "threads": threads,
        "events": events,
        "away": away
    }))
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
            thread_ids: threads
                .iter()
                .map(|thread| thread.thread_id.clone())
                .collect(),
            labels: threads.iter().map(|thread| thread.label.clone()).collect(),
            applied_filters: json!({ "project": project_filter, "limit": limit }),
        },
        threads,
    })
}

fn list_cached_threads(conn: &Connection, limit: u64) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT t.thread_id, t.name, t.cwd, t.updated_at, t.status_type, t.status_flags_json,
                t.last_turn_status, t.last_preview, p.prompt_id, p.prompt_kind, p.prompt_status, p.question
         FROM threads_cache t
         LEFT JOIN pending_prompts p ON p.thread_id = t.thread_id
         ORDER BY COALESCE(t.updated_at, 0) DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![to_sql_i64(limit)?], |row| {
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
            row.get::<_, Option<String>>(11)?,
        ))
    })?;
    let raw = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
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
                prompt_id,
                prompt_kind,
                prompt_status,
                question,
            )| {
                Ok(json!({
                    "threadId": thread_id,
                    "name": name,
                    "cwd": cwd,
                    "updatedAt": optional_from_sql_i64(updated_at_raw)?,
                    "statusType": status_type,
                    "statusFlags": serde_json::from_str::<Value>(&status_flags_json).unwrap_or(json!([])),
                    "lastTurnStatus": last_turn_status,
                    "lastPreview": last_preview,
                    "pendingPrompt": match (prompt_id, prompt_kind, prompt_status, question) {
                        (Some(prompt_id), Some(prompt_kind), Some(prompt_status), Some(question)) => json!({
                            "promptId": prompt_id,
                            "promptKind": prompt_kind,
                            "promptStatus": prompt_status,
                            "question": question
                        }),
                        _ => Value::Null,
                    }
                }))
            },
        )
        .collect()
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
        "SELECT t.thread_id, t.name, t.cwd, t.updated_at, t.last_seen_at, t.status_type, t.status_flags_json,
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
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<String>>(11)?,
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
                last_seen_at_raw,
                status_type,
                status_flags_json,
                last_turn_status,
                last_preview,
                prompt_kind,
                prompt_status,
                question,
            )| {
                let status_flags =
                    serde_json::from_str::<Vec<String>>(&status_flags_json).unwrap_or_default();
                let pending_prompt = prompt_kind.clone().map(|kind| PendingPrompt {
                    prompt_id: format!("{}:{}", kind, thread_id),
                    kind,
                    status: prompt_status.unwrap_or_else(|| "Needs input".to_string()),
                    question,
                });
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
                let mut item = classify_inbox_item(&snapshot, now);
                item.last_seen_at = Some(from_sql_i64(last_seen_at_raw)?);
                item.recent_action = recent_actions_json(conn, &item.thread_id, 1)?
                    .into_iter()
                    .next();
                Ok(item)
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

    items.sort_by_key(|item| std::cmp::Reverse(score_inbox_item(item)));
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

fn normalized_message(message: Option<&str>) -> Option<String> {
    message
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn text_input_value(text: &str) -> Value {
    json!({ "type": "text", "text": text, "text_elements": [] })
}

fn thread_start_params(cwd: Option<&str>, message: Option<&str>) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(cwd) = cwd.filter(|value| !value.trim().is_empty()) {
        params.insert("cwd".to_string(), json!(cwd));
    }
    if let Some(message) = normalized_message(message) {
        params.insert("input".to_string(), json!([text_input_value(&message)]));
    }
    Value::Object(params)
}

fn turn_start_params(thread_id: &str, cwd: Option<&str>, message: &str) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("threadId".to_string(), json!(thread_id));
    if let Some(cwd) = cwd.filter(|value| !value.trim().is_empty()) {
        params.insert("cwd".to_string(), json!(cwd));
    }
    params.insert("input".to_string(), json!([text_input_value(message)]));
    Value::Object(params)
}

fn thread_field_string(response: &Value, field: &str) -> Option<String> {
    response
        .get("thread")
        .and_then(|thread| thread.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn thread_id_from_response(response: &Value) -> Option<String> {
    thread_field_string(response, "id")
}

fn thread_cwd_from_response(response: &Value, fallback: Option<&str>) -> Option<String> {
    thread_field_string(response, "cwd").or_else(|| fallback.map(str::to_string))
}

fn new_thread_live_result(
    cwd: Option<&str>,
    message: Option<&str>,
    created: Value,
    started: Option<Value>,
) -> Value {
    let thread_id = thread_id_from_response(&created);
    let resolved_cwd = thread_cwd_from_response(&created, cwd);
    json!({
        "ok": true,
        "action": "new",
        "threadId": thread_id,
        "cwd": resolved_cwd,
        "message": normalized_message(message),
        "created": created,
        "started": started
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

fn fork_thread_live_result(
    from_thread_id: &str,
    message: Option<&str>,
    forked: Value,
    started: Option<Value>,
) -> Value {
    json!({
        "ok": true,
        "action": "fork",
        "fromThreadId": from_thread_id,
        "threadId": thread_id_from_response(&forked),
        "message": normalized_message(message),
        "forked": forked,
        "started": started
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ArchiveSelection {
    targets: Vec<String>,
    using_filter_selection: bool,
}

fn resolve_archive_targets(
    conn: &Connection,
    thread_ids: &[String],
    project: Option<&str>,
    status: Option<&str>,
    attention: Option<&str>,
    limit: u64,
    now: u64,
) -> Result<ArchiveSelection> {
    if !thread_ids.is_empty() {
        return Ok(ArchiveSelection {
            targets: thread_ids.to_vec(),
            using_filter_selection: false,
        });
    }

    let inbox = list_inbox_from_db(conn, now, project, status, attention, None, limit)?;
    Ok(ArchiveSelection {
        targets: inbox
            .items
            .into_iter()
            .map(|item| item.thread_id)
            .collect::<Vec<_>>(),
        using_filter_selection: true,
    })
}

fn archive_result(dry_run: bool, results: Vec<Value>) -> Value {
    json!({
        "ok": true,
        "action": "archive",
        "dryRun": dry_run,
        "results": results
    })
}

#[cfg(test)]
fn archive_from_db(
    conn: &Connection,
    thread_id: Option<&str>,
    attention: Option<&str>,
    dry_run: bool,
    yes: bool,
    now: u64,
) -> Result<Value> {
    let explicit = thread_id
        .map(|value| vec![value.to_string()])
        .unwrap_or_default();
    let selection = resolve_archive_targets(conn, &explicit, None, None, attention, 100, now)?;
    if !dry_run && selection.using_filter_selection && !yes {
        bail!("Refusing bulk archive without --yes or --dry-run");
    }

    let results = selection
        .targets
        .into_iter()
        .map(|thread_id| {
            json!({
                "threadId": thread_id,
                "status": if dry_run { "would_archive" } else { "archived" }
            })
        })
        .collect::<Vec<_>>();

    Ok(archive_result(dry_run, results))
}

fn sync_state_from_live(
    client: &mut CodexAppServerClient,
    conn: &Connection,
    now: u64,
    limit: u64,
    record_deliveries: bool,
) -> Result<Value> {
    let list = client.request(
        "thread/list",
        json!({
            "limit": limit,
            "sortKey": "updated_at"
        }),
    )?;
    let mut snapshots = Vec::new();
    if let Some(summaries) = list.get("data").and_then(Value::as_array) {
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
            let snapshot = normalize_thread_snapshot(summary, thread)?;
            snapshots.push(snapshot);
        }
    }
    reconcile_thread_snapshots(conn, now, snapshots, record_deliveries)
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

    if let Ok(path_codex) = which::which("codex") {
        push_candidate(&mut candidates, &mut seen, path_codex, "path");
    }

    for candidate in candidates {
        if codex_candidate_is_usable(&candidate.path) {
            return Ok(candidate);
        }
    }

    bail!(
        "Could not resolve codex executable. Set CODEX_BIN or install Codex so `codex --version` works in this environment"
    )
}

fn codex_candidate_is_usable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }
    codex_candidate_is_usable_with_timeout(path, Duration::from_secs(3))
}

fn codex_candidate_is_usable_with_timeout(path: &Path, timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
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

struct NotificationMessage<'a> {
    kind: &'a str,
    display_name: &'a str,
    project: &'a str,
    cwd: Option<&'a str>,
    summary: Option<&'a str>,
    detail: Option<&'a str>,
    recent_action: Option<&'a str>,
    next_step: Option<&'a str>,
}

fn render_notification_message(message: NotificationMessage<'_>) -> String {
    let NotificationMessage {
        kind,
        display_name,
        project,
        cwd,
        summary,
        detail,
        recent_action,
        next_step,
    } = message;
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

fn describe_recent_action(action_type: Option<&str>) -> Option<String> {
    match action_type {
        Some("reply") => Some("Last action: replied".to_string()),
        Some("approve") => Some("Last action: approved".to_string()),
        Some("fork") => Some("Last action: forked thread".to_string()),
        Some("new") => Some("Last action: started thread".to_string()),
        Some("archive") => Some("Last action: archived".to_string()),
        Some("unarchive") => Some("Last action: unarchived".to_string()),
        Some(other) => Some(format!("Last action: {other}")),
        None => None,
    }
}

fn recent_action_label(action: Option<&Value>) -> Option<String> {
    describe_recent_action(
        action
            .and_then(|value| value.get("actionType"))
            .and_then(Value::as_str),
    )
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
    let away_session_id =
        existing_session.or_else(|| away_started_at.map(|value| value.to_string()));
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
        conn.execute(
            "DELETE FROM settings WHERE key = ?1",
            params!["away_started_at"],
        )?;
        conn.execute(
            "DELETE FROM settings WHERE key = ?1",
            params!["away_session_id"],
        )?;
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

fn build_show_thread_result(
    conn: Option<&Connection>,
    requested_thread_id: &str,
    response: Value,
) -> Result<Value> {
    let thread = response.get("thread").cloned().unwrap_or(response);
    let thread_id = thread
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(requested_thread_id)
        .to_string();
    let name = thread
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = thread
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let project = derive_project_label(cwd.as_deref());
    let status_type = thread
        .pointer("/status/type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let status_flags = thread
        .pointer("/status/activeFlags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let last_turn_status = turns
        .last()
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let last_agent_message = find_last_message_by_type(&thread, "agentMessage");
    let last_user_message = find_last_message_by_type(&thread, "userMessage");
    let latest_question = latest_question(&thread);
    let pending_prompt = show_pending_prompt(&status_flags);
    let can_approve = pending_prompt
        .as_ref()
        .and_then(|prompt| prompt.get("kind"))
        .and_then(Value::as_str)
        == Some("approval");
    let can_reply = pending_prompt
        .as_ref()
        .and_then(|prompt| prompt.get("kind"))
        .and_then(Value::as_str)
        == Some("reply")
        || !can_approve;
    let is_waiting = pending_prompt.is_some();
    let is_completed = last_turn_status.as_deref() == Some("completed");
    let display_name = derive_thread_display_name(
        name.as_deref(),
        project.as_deref(),
        latest_question.as_deref().or(last_user_message.as_deref()),
        &thread_id,
    );
    let delta_summary = build_delta_summary(
        pending_prompt.as_ref(),
        last_turn_status.as_deref(),
        last_agent_message.as_deref(),
    );
    let unresolved_summary = build_unresolved_summary(
        pending_prompt.as_ref(),
        latest_question.as_deref(),
        last_user_message.as_deref(),
    );
    let recent_actions = match conn {
        Some(conn) => recent_actions_json(conn, &thread_id, 5)?,
        None => Vec::new(),
    };

    Ok(json!({
        "thread": {
            "threadId": thread_id,
            "name": name,
            "displayName": display_name,
            "project": project,
            "cwd": cwd,
            "statusType": status_type,
            "statusFlags": status_flags,
            "pendingPrompt": pending_prompt,
            "canReply": can_reply,
            "canApprove": can_approve,
            "isWaiting": is_waiting,
            "isCompleted": is_completed,
            "lastTurnStatus": last_turn_status,
            "lastAgentMessage": last_agent_message,
            "lastUserMessage": last_user_message,
            "latestQuestion": latest_question,
            "deltaSummary": delta_summary,
            "unresolvedSummary": unresolved_summary,
            "recentActions": recent_actions,
            "turns": turns
        }
    }))
}

fn get_status_audit(conn: &Connection, thread_id: Option<&str>) -> Result<Value> {
    let inbox = list_inbox_from_db(conn, now_millis()?, None, None, None, None, 1000)?;
    let audits = inbox
        .items
        .into_iter()
        .filter(|item| {
            thread_id
                .map(|target| item.thread_id == target)
                .unwrap_or(true)
        })
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

fn classify_app_server_error_message(message: &str) -> Value {
    if message.contains("thread not loaded") {
        return json!({
            "code": "thread_not_loaded",
            "retryable": true,
            "message": message,
            "guidance": "The thread is not visible in this app-server client session. Prefer single-session flows like --follow on new/reply/approve/fork."
        });
    }

    if message.contains("is not materialized yet")
        || message.contains("includeTurns is unavailable before first user message")
    {
        return json!({
            "code": "thread_not_materialized",
            "retryable": true,
            "message": message,
            "guidance": "The thread exists but full turn materialization is not ready yet. Retry shortly or use single-session follow after creating/resuming the thread."
        });
    }

    if message.contains("no rollout found for thread id") {
        return json!({
            "code": "thread_no_rollout",
            "retryable": false,
            "message": message,
            "guidance": "The backend does not consider this thread forkable/rollable in the current context. This is likely an upstream limitation rather than a bridge bug."
        });
    }

    if message.contains("requires experimentalApi capability") {
        return json!({
            "code": "experimental_api_required",
            "retryable": true,
            "message": message,
            "guidance": "Retry with --experimental-realtime or a client initialized with experimentalApi: true."
        });
    }

    json!({
        "code": "app_server_error",
        "retryable": false,
        "message": message,
        "guidance": Value::Null
    })
}

fn step_result(label: &str, result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({ "ok": true, "step": label, "result": value }),
        Err(error) => {
            let message = format!("{error:#}");
            json!({
                "ok": false,
                "step": label,
                "error": classify_app_server_error_message(&message)
            })
        }
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

fn parse_event_filter(input: Option<&str>) -> Option<BTreeSet<String>> {
    input
        .map(|value| {
            value
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect::<BTreeSet<_>>()
        })
        .filter(|set| !set.is_empty())
}

fn should_emit_event(filter: Option<&BTreeSet<String>>, event: &Value) -> bool {
    match filter {
        None => true,
        Some(filter) => event
            .get("type")
            .and_then(Value::as_str)
            .map(|kind| filter.contains(kind))
            .unwrap_or(false),
    }
}

fn normalize_notification_event(notification: &Value) -> Value {
    match notification.get("method").and_then(Value::as_str) {
        Some("thread/status/changed") => json!({
            "type": "thread_status_changed",
            "threadId": notification.pointer("/params/threadId").cloned().unwrap_or(Value::Null),
            "status": notification.pointer("/params/status").cloned().unwrap_or(Value::Null),
            "raw": notification
        }),
        Some("turn/started") => json!({
            "type": "turn_started",
            "threadId": notification.pointer("/params/threadId").cloned().unwrap_or(Value::Null),
            "turn": notification.pointer("/params/turn").cloned().unwrap_or(Value::Null),
            "raw": notification
        }),
        Some("item/started") => json!({
            "type": "item_started",
            "threadId": notification.pointer("/params/threadId").cloned().unwrap_or(Value::Null),
            "turnId": notification.pointer("/params/turnId").cloned().unwrap_or(Value::Null),
            "item": notification.pointer("/params/item").cloned().unwrap_or(Value::Null),
            "raw": notification
        }),
        Some("item/completed") => json!({
            "type": "item_completed",
            "threadId": notification.pointer("/params/threadId").cloned().unwrap_or(Value::Null),
            "turnId": notification.pointer("/params/turnId").cloned().unwrap_or(Value::Null),
            "item": notification.pointer("/params/item").cloned().unwrap_or(Value::Null),
            "raw": notification
        }),
        _ => json!({
            "type": "notification",
            "raw": notification
        }),
    }
}

fn push_follow_event(
    events: &mut Vec<Value>,
    event_filter: Option<&BTreeSet<String>>,
    event: Value,
) {
    if should_emit_event(event_filter, &event) {
        events.push(event);
    }
}

fn read_thread_with_turns_fallback(
    client: &mut CodexAppServerClient,
    thread_id: &str,
) -> Result<Value> {
    match client.request(
        "thread/read",
        json!({ "threadId": thread_id, "includeTurns": true }),
    ) {
        Ok(value) => Ok(value),
        Err(error) => {
            let message = format!("{error:#}");
            if !message.contains("includeTurns is unavailable before first user message")
                && !message.contains("is not materialized yet")
            {
                return Err(error);
            }
            client.request(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": false }),
            )
        }
    }
}

fn collect_follow_events(
    client: &mut CodexAppServerClient,
    thread_id: &str,
    message: Option<&str>,
    duration_ms: u64,
    poll_interval_ms: u64,
    experimental_realtime: bool,
    event_filter: Option<&BTreeSet<String>>,
) -> Result<Vec<Value>> {
    let mut events = Vec::new();
    push_follow_event(
        &mut events,
        event_filter,
        json!({
            "type": "follow_started",
            "threadId": thread_id,
            "durationMs": duration_ms,
            "pollIntervalMs": poll_interval_ms,
            "experimentalRealtime": experimental_realtime
        }),
    );

    let initial = read_thread_with_turns_fallback(client, thread_id)?;
    push_follow_event(
        &mut events,
        event_filter,
        json!({
            "type": "follow_snapshot",
            "threadId": thread_id,
            "thread": initial.get("thread").cloned().unwrap_or(Value::Null)
        }),
    );

    if experimental_realtime {
        match client.request("thread/realtime/start", json!({ "threadId": thread_id })) {
            Ok(realtime) => push_follow_event(
                &mut events,
                event_filter,
                json!({ "type": "follow_realtime_started", "threadId": thread_id, "realtime": realtime }),
            ),
            Err(error) => push_follow_event(
                &mut events,
                event_filter,
                json!({
                    "type": "follow_realtime_error",
                    "threadId": thread_id,
                    "message": format!("{error:#}")
                }),
            ),
        }
    }

    if let Some(message) = message.map(str::trim).filter(|value| !value.is_empty()) {
        let started = client.request(
            "turn/start",
            json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": message, "text_elements": [] }]
            }),
        )?;
        push_follow_event(
            &mut events,
            event_filter,
            json!({ "type": "follow_turn_started", "threadId": thread_id, "started": started }),
        );
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(duration_ms);
    let mut next_poll_at =
        std::time::Instant::now() + std::time::Duration::from_millis(poll_interval_ms);
    while std::time::Instant::now() < deadline {
        for notification in client.drain_notifications() {
            push_follow_event(
                &mut events,
                event_filter,
                normalize_notification_event(&notification),
            );
        }

        if poll_interval_ms > 0 && std::time::Instant::now() >= next_poll_at {
            let thread = read_thread_with_turns_fallback(client, thread_id)?;
            push_follow_event(
                &mut events,
                event_filter,
                json!({
                    "type": "follow_snapshot",
                    "threadId": thread_id,
                    "thread": thread.get("thread").cloned().unwrap_or(Value::Null)
                }),
            );
            next_poll_at =
                std::time::Instant::now() + std::time::Duration::from_millis(poll_interval_ms);
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    for notification in client.drain_notifications() {
        push_follow_event(
            &mut events,
            event_filter,
            normalize_notification_event(&notification),
        );
    }

    if experimental_realtime {
        match client.request("thread/realtime/stop", json!({ "threadId": thread_id })) {
            Ok(realtime) => push_follow_event(
                &mut events,
                event_filter,
                json!({ "type": "follow_realtime_stopped", "threadId": thread_id, "realtime": realtime }),
            ),
            Err(error) => push_follow_event(
                &mut events,
                event_filter,
                json!({
                    "type": "follow_realtime_stop_error",
                    "threadId": thread_id,
                    "message": format!("{error:#}")
                }),
            ),
        }
    }

    Ok(events)
}

fn persist_follow_events(
    conn: &Connection,
    thread_id: &str,
    events: &[Value],
    now: u64,
) -> Result<()> {
    let latest_snapshot = events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("follow_snapshot"))
        .and_then(|event| event.get("thread"));

    if let Some(thread) = latest_snapshot {
        let snapshot = normalize_thread_snapshot(thread, thread)?;
        upsert_thread_snapshot(conn, &snapshot, now)?;
    }

    if events.iter().any(|event| {
        matches!(
            event.get("type").and_then(Value::as_str),
            Some("task_complete") | Some("turn.completed")
        )
    }) {
        conn.execute(
            "DELETE FROM pending_prompts WHERE thread_id = ?1",
            params![thread_id],
        )?;
    }

    Ok(())
}

struct FollowRun<'a> {
    thread_id: &'a str,
    duration_ms: u64,
    poll_interval_ms: u64,
    experimental_realtime: bool,
    event_filter: Option<&'a BTreeSet<String>>,
    stream: bool,
}

fn follow_result_summary(
    thread_id: &str,
    duration_ms: u64,
    experimental_realtime: bool,
    events: &[Value],
    include_events: bool,
) -> Value {
    let mut summary = json!({
        "ok": true,
        "action": "follow",
        "threadId": thread_id,
        "durationMs": duration_ms,
        "experimentalRealtime": experimental_realtime
    });
    if include_events {
        summary["events"] = json!(events);
    }
    summary
}

fn attach_follow_payload(mut result: Value, follow: Value, stream: bool) -> Value {
    result["follow"] = follow;
    if stream {
        json!({
            "type": "command_result",
            "result": result
        })
    } else {
        result
    }
}

fn composed_settle_duration_ms(duration_ms: u64) -> u64 {
    if duration_ms <= 100 {
        0
    } else {
        20_000
    }
}

fn event_is_terminal(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(Value::as_str),
        Some("item_completed")
            | Some("task_complete")
            | Some("turn.completed")
            | Some("turn.waiting")
            | Some("approval_request")
    )
}

fn thread_snapshot_is_terminal(thread: &Value) -> bool {
    let last_turn_status = thread
        .get("turns")
        .and_then(Value::as_array)
        .and_then(|turns| turns.last())
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str);
    if last_turn_status == Some("completed") {
        return true;
    }

    thread
        .pointer("/status/activeFlags")
        .and_then(Value::as_array)
        .map(|active_flags| {
            active_flags.iter().any(|flag| {
                matches!(
                    flag.as_str(),
                    Some("waitingOnUserInput") | Some("waitingOnInput") | Some("waitingOnApproval")
                )
            })
        })
        .unwrap_or(false)
}

fn settle_composed_follow_events(
    client: &mut CodexAppServerClient,
    thread_id: &str,
    events: &mut Vec<Value>,
    duration_ms: u64,
    event_filter: Option<&BTreeSet<String>>,
) -> Result<()> {
    if events.iter().any(event_is_terminal) {
        return Ok(());
    }

    let settle_duration_ms = composed_settle_duration_ms(duration_ms);
    if settle_duration_ms == 0 {
        return Ok(());
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(settle_duration_ms);
    while std::time::Instant::now() < deadline {
        let read = read_thread_with_turns_fallback(client, thread_id)?;
        let thread = read.get("thread").cloned().unwrap_or(Value::Null);
        push_follow_event(
            events,
            event_filter,
            json!({
                "type": "follow_snapshot",
                "threadId": thread_id,
                "thread": thread
            }),
        );
        if read
            .get("thread")
            .map(thread_snapshot_is_terminal)
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1000));
    }

    Ok(())
}

fn attach_follow_result(
    result: Value,
    client: &mut CodexAppServerClient,
    conn: &Connection,
    follow: FollowRun<'_>,
) -> Result<Value> {
    let mut events = collect_follow_events(
        client,
        follow.thread_id,
        None,
        follow.duration_ms,
        follow.poll_interval_ms,
        follow.experimental_realtime,
        follow.event_filter,
    )?;
    if follow.stream {
        for event in &events {
            println!("{}", serde_json::to_string(event)?);
        }
        let follow_summary = follow_result_summary(
            follow.thread_id,
            follow.duration_ms,
            follow.experimental_realtime,
            &events,
            false,
        );
        return Ok(attach_follow_payload(result, follow_summary, true));
    }

    settle_composed_follow_events(
        client,
        follow.thread_id,
        &mut events,
        follow.duration_ms,
        follow.event_filter,
    )?;
    persist_follow_events(conn, follow.thread_id, &events, now_millis()?)?;
    let follow_summary = follow_result_summary(
        follow.thread_id,
        follow.duration_ms,
        follow.experimental_realtime,
        &events,
        true,
    );
    Ok(attach_follow_payload(result, follow_summary, false))
}

#[cfg(test)]
struct FollowEventsFixture<'a> {
    thread_id: &'a str,
    duration_ms: u64,
    poll_interval_ms: u64,
    experimental_realtime: bool,
    initial_thread: Option<Value>,
    started: Option<Value>,
    notifications: Vec<Value>,
    event_filter: Option<&'a BTreeSet<String>>,
}

#[cfg(test)]
fn build_follow_events(fixture: FollowEventsFixture<'_>) -> Result<Vec<Value>> {
    let mut events = vec![json!({
        "type": "follow_started",
        "threadId": fixture.thread_id,
        "durationMs": fixture.duration_ms,
        "pollIntervalMs": fixture.poll_interval_ms,
        "experimentalRealtime": fixture.experimental_realtime
    })];
    if let Some(thread) = fixture.initial_thread {
        events.push(json!({
            "type": "follow_snapshot",
            "threadId": fixture.thread_id,
            "thread": thread
        }));
    }
    if let Some(started) = fixture.started {
        events.push(json!({
            "type": "follow_turn_started",
            "threadId": fixture.thread_id,
            "started": started
        }));
    }
    events.extend(
        fixture
            .notifications
            .into_iter()
            .map(|notification| normalize_notification_event(&notification)),
    );
    Ok(events
        .into_iter()
        .filter(|event| should_emit_event(fixture.event_filter, event))
        .collect())
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

fn watch_thread_error_event(error: &anyhow::Error) -> Value {
    json!({
        "type": "thread_error",
        "message": error.to_string()
    })
}

fn enrich_event_with_thread(event: Value, threads: &[Value]) -> Value {
    let Some(thread_id) = event.get("threadId").and_then(Value::as_str) else {
        return event;
    };
    let Some(thread) = threads
        .iter()
        .find(|thread| thread.get("threadId").and_then(Value::as_str) == Some(thread_id))
    else {
        return event;
    };
    let mut enriched = event;
    if let Some(object) = enriched.as_object_mut() {
        object.insert("thread".to_string(), thread.clone());
    }
    enriched
}

fn watch_events_from_sync_result(
    sync_result: &Value,
    notifications: Vec<Value>,
    filter: Option<&BTreeSet<String>>,
) -> Vec<Value> {
    let threads = sync_result
        .get("threads")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut events = sync_result
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|event| enrich_event_with_thread(event, &threads))
        .collect::<Vec<_>>();
    events.extend(
        notifications
            .into_iter()
            .map(|notification| normalize_notification_event(&notification))
            .map(|event| enrich_event_with_thread(event, &threads)),
    );
    filter_watch_events(events, filter)
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

struct CodexWatchReceiver {
    rx: std::sync::mpsc::Receiver<()>,
    _watcher: notify::RecommendedWatcher,
}

impl CodexWatchReceiver {
    fn recv_timeout(&self, timeout: std::time::Duration) {
        let _ = self.rx.recv_timeout(timeout);
        while self.rx.try_recv().is_ok() {}
    }
}

fn start_codex_watch_receiver() -> Result<CodexWatchReceiver> {
    let home = PathBuf::from(env::var("HOME").context("HOME is not set")?);
    let sessions_root = home.join(".codex").join("sessions");
    let session_index = home.join(".codex").join("session_index.jsonl");
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(
        move |result: notify::Result<notify::Event>| {
            if result.is_ok() {
                let _ = tx.send(());
            }
        },
        notify::Config::default(),
    )?;

    let mut watched_any = false;
    if sessions_root.exists() {
        watcher.watch(&sessions_root, notify::RecursiveMode::Recursive)?;
        watched_any = true;
    }
    if session_index.exists() {
        watcher.watch(&session_index, notify::RecursiveMode::NonRecursive)?;
        watched_any = true;
    }

    if !watched_any {
        bail!("No Codex session paths exist to watch");
    }

    Ok(CodexWatchReceiver {
        rx,
        _watcher: watcher,
    })
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
        format!(
            "{}…",
            trimmed
                .chars()
                .take(max_length.saturating_sub(1))
                .collect::<String>()
        )
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
                cwd = parsed
                    .pointer("/payload/cwd")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                meta_timestamp = parsed
                    .pointer("/payload/timestamp")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                continue;
            }
            if parsed.get("type").and_then(Value::as_str) != Some("response_item") {
                continue;
            }
            let role = parsed
                .pointer("/payload/role")
                .and_then(Value::as_str)
                .unwrap_or("");
            let pieces = parsed
                .pointer("/payload/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
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
            let line_score = tokens
                .iter()
                .filter(|token| lower.contains(token.as_str()))
                .count() as u64;
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
                if normalized.contains("should")
                    || normalized.contains("need")
                    || normalized.contains("verified")
                {
                    decisions.push(truncate_text(&normalized, 180));
                }
            }
        }
        if score == 0 {
            continue;
        }
        sessions.push((
            score,
            json!({
                "session_id": session_id,
                "thread_name": entry.get("thread_name").cloned().unwrap_or(Value::Null),
                "cwd": cwd,
                "updated_at": meta_timestamp.or_else(|| Some(updated_at.to_string())),
                "why_relevant": why_relevant.into_iter().take(3).collect::<Vec<_>>(),
                "user_asks": user_asks.into_iter().take(3).collect::<Vec<_>>(),
                "decisions": decisions.into_iter().take(3).collect::<Vec<_>>(),
                "receipts": receipts.into_iter().take(3).collect::<Vec<_>>(),
                "suggested_show_command": format!("codex-recall show {session_id} --limit 40")
            }),
        ));
    }
    sessions.sort_by(|a, b| b.0.cmp(&a.0));
    let limited = sessions
        .into_iter()
        .take(limit.clamp(1, 3) as usize)
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    Ok(json!({ "query": query, "sessions": limited }))
}

fn build_notify_away_from_db(
    conn: &Connection,
    now: u64,
    include_completed: bool,
    mark_delivered: bool,
) -> Result<Value> {
    let away_status = get_away_mode(conn)?;
    let away = away_status
        .get("away")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let away_session_id = away_status
        .get("awaySessionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let away_started_at = away_status
        .get("awayStartedAt")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if !away || away_session_id.is_none() {
        return Ok(json!({ "ok": true, "action": "notify-away", "notifications": [] }));
    }
    let away_session_id = away_session_id.unwrap();

    let waiting = list_waiting_from_db(conn, None, 1_000)?;
    let inbox = list_inbox_from_db(conn, now, None, None, None, None, 10_000)?;
    let mut candidates = Vec::new();

    for thread in waiting.threads {
        if thread.updated_at.unwrap_or(0) < away_started_at {
            continue;
        }
        let delivery_key = format!(
            "{}:thread_waiting:{}:{}:{}:{}",
            away_session_id,
            thread.thread_id,
            thread.prompt.kind,
            thread.prompt.status,
            thread.prompt.question.clone().unwrap_or_default()
        );
        candidates.push(NotificationCandidate {
            delivery_key,
            kind: "thread_waiting",
            thread_id: thread.thread_id.clone(),
            text: render_notification_message(NotificationMessage {
                kind: "thread_waiting",
                display_name: &thread.display_name,
                project: thread.project.as_deref().unwrap_or("unknown"),
                cwd: thread.cwd.as_deref(),
                summary: thread.prompt.question.as_deref(),
                detail: Some(&thread.prompt.kind),
                recent_action: recent_action_label(
                    recent_actions_json(conn, &thread.thread_id, 1)?.first(),
                )
                .as_deref(),
                next_step: Some("Tell me how you want me to reply"),
            }),
        });
    }

    for item in inbox.items {
        let effective_updated_at = item
            .updated_at
            .unwrap_or(0)
            .max(item.last_seen_at.unwrap_or(0));
        if effective_updated_at < away_started_at {
            continue;
        }
        if item.attention_reason == "completed" && !include_completed {
            continue;
        }
        if item.attention_reason == "needs_reply" || item.attention_reason == "pending_approval" {
            continue;
        }
        let seen_during_away = item.last_seen_at.unwrap_or(0) >= away_started_at;
        let has_preview = item
            .delivery_preview
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let kind = if item.attention_reason == "completed" {
            "thread_completed"
        } else if item.attention_reason == "updated"
            || (seen_during_away && has_preview && item.attention_reason == "active")
        {
            "thread_updated"
        } else {
            continue;
        };
        let delivery_key = format!(
            "{}:{}:{}:{}:{}",
            away_session_id,
            kind,
            item.thread_id,
            item.updated_at.unwrap_or(0),
            item.delivery_preview.clone().unwrap_or_default()
        );
        let summary = if kind == "thread_completed" || kind == "thread_updated" {
            preserve_message_body(item.delivery_preview.as_deref(), 20_000)
        } else {
            item.last_preview.clone()
        };
        candidates.push(NotificationCandidate {
            delivery_key,
            kind,
            thread_id: item.thread_id.clone(),
            text: render_notification_message(NotificationMessage {
                kind,
                display_name: &item.display_name,
                project: item.project.as_deref().unwrap_or("unknown"),
                cwd: item.cwd.as_deref(),
                summary: summary.as_deref(),
                detail: None,
                recent_action: recent_action_label(item.recent_action.as_ref()).as_deref(),
                next_step: Some(if kind == "thread_completed" {
                    "Tell me if you want me to follow up"
                } else {
                    "Tell me if you want me to check it"
                }),
            }),
        });
    }

    let raw = match conn.prepare(
        "SELECT event_key, thread_id, event_type, observed_at, payload_json
         FROM thread_events
         WHERE observed_at >= ?1
         ORDER BY observed_at ASC",
    ) {
        Ok(mut stmt) => {
            let rows = stmt.query_map(params![to_sql_i64(away_started_at)?], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        }
        Err(error) => {
            let message = error.to_string();
            if message.contains("no such table: thread_events") {
                Vec::new()
            } else {
                return Err(error.into());
            }
        }
    };
    for (event_key, thread_id, event_type, _observed_at, payload_json) in raw {
        if event_type == "thread_completed" && !include_completed {
            continue;
        }
        let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
        let summary = payload.get("lastPreview").and_then(Value::as_str).map(str::to_string);
        let exists = candidates.iter().any(|candidate| candidate.delivery_key == event_key);
        if exists {
            continue;
        }
        let item = conn
            .query_row(
                "SELECT name, cwd FROM threads_cache WHERE thread_id = ?1",
                params![thread_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()?;
        let (name, cwd) = item.unwrap_or((None, None));
        let project = derive_project_label(cwd.as_deref()).unwrap_or_else(|| "unknown".to_string());
        let display_name = derive_thread_display_name(
            name.as_deref(),
            Some(project.as_str()),
            summary.as_deref(),
            &thread_id,
        );
        let kind = if event_type == "thread_completed" {
            "thread_completed"
        } else {
            "thread_waiting"
        };
        candidates.push(NotificationCandidate {
            delivery_key: event_key,
            kind,
            thread_id: thread_id.clone(),
            text: render_notification_message(NotificationMessage {
                kind,
                display_name: &display_name,
                project: &project,
                cwd: cwd.as_deref(),
                summary: summary.as_deref(),
                detail: None,
                recent_action: recent_action_label(recent_actions_json(conn, &thread_id, 1)?.first())
                    .as_deref(),
                next_step: Some(if kind == "thread_completed" {
                    "Tell me if you want me to follow up"
                } else {
                    "Tell me how you want me to reply"
                }),
            }),
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
        if mark_delivered {
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
        }
        notifications.push(json!({
            "type": candidate.kind,
            "threadId": candidate.thread_id,
            "text": candidate.text
        }));
    }

    Ok(json!({ "ok": true, "action": "notify-away", "notifications": notifications }))
}

#[cfg(test)]
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
    messages: Receiver<AppServerReaderMessage>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
    notifications: Vec<Value>,
    reader_error: Option<String>,
}

enum AppServerReaderMessage {
    Json(Value),
    Error(String),
    Closed,
}

impl CodexAppServerClient {
    fn connect() -> Result<Self> {
        Self::connect_with_options(false)
    }

    fn connect_with_options(experimental_api: bool) -> Result<Self> {
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

        let (messages, reader) = spawn_app_server_reader(stdout);

        let mut client = Self {
            child,
            stdin,
            messages,
            reader: Some(reader),
            next_id: 1,
            notifications: Vec::new(),
            reader_error: None,
        };

        let _ = client.request("initialize", initialize_params(experimental_api))?;
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
            if let Some(error) = self.reader_error.take() {
                bail!("{error}");
            }
            match self.messages.recv() {
                Ok(AppServerReaderMessage::Json(parsed)) => {
                    if let Some(result) =
                        handle_app_server_message(&parsed, id, &mut self.notifications)?
                    {
                        return Ok(result);
                    }
                }
                Ok(AppServerReaderMessage::Error(message)) => bail!("{message}"),
                Ok(AppServerReaderMessage::Closed) | Err(_) => {
                    bail!("codex app-server closed stdout while waiting for {method}")
                }
            }
        }
    }

    fn drain_notifications(&mut self) -> Vec<Value> {
        while let Ok(message) = self.messages.try_recv() {
            match message {
                AppServerReaderMessage::Json(parsed) => {
                    if parsed.get("id").is_none()
                        && parsed.get("method").and_then(Value::as_str).is_some()
                    {
                        self.notifications.push(parsed);
                    }
                }
                AppServerReaderMessage::Error(message) => {
                    if self.reader_error.is_none() {
                        self.reader_error = Some(message);
                    }
                }
                AppServerReaderMessage::Closed => {}
            }
        }
        std::mem::take(&mut self.notifications)
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        writeln!(self.stdin, "{}", serde_json::to_string(value)?)?;
        self.stdin.flush()?;
        Ok(())
    }
}

fn spawn_app_server_reader(
    stdout: ChildStdout,
) -> (Receiver<AppServerReaderMessage>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(AppServerReaderMessage::Closed);
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(parsed) => {
                            let _ = tx.send(AppServerReaderMessage::Json(parsed));
                        }
                        Err(error) => {
                            let _ = tx.send(AppServerReaderMessage::Error(format!(
                                "invalid JSON from app-server: {trimmed}: {error}"
                            )));
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(AppServerReaderMessage::Error(format!(
                        "failed to read app-server stdout: {error}"
                    )));
                    break;
                }
            }
        }
    });
    (rx, reader)
}

fn initialize_params(experimental_api: bool) -> Value {
    let capabilities = if experimental_api {
        json!({ "experimentalApi": true })
    } else {
        json!({})
    };

    json!({
        "protocolVersion": 1,
        "capabilities": capabilities,
        "clientInfo": {
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn handle_app_server_message(
    parsed: &Value,
    expected_id: u64,
    notifications: &mut Vec<Value>,
) -> Result<Option<Value>> {
    match parsed.get("id") {
        Some(value) if value == &json!(expected_id) => {
            if let Some(error) = parsed.get("error") {
                bail!("{}", error);
            }
            Ok(Some(parsed.get("result").cloned().unwrap_or(Value::Null)))
        }
        Some(_) => Ok(None),
        None => {
            if parsed.get("method").and_then(Value::as_str).is_some() {
                notifications.push(parsed.clone());
            }
            Ok(None)
        }
    }
}

impl Drop for CodexAppServerClient {
    fn drop(&mut self) {
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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
            "codex-hermes-bridge",
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
        assert!(
            new_cli.is_ok(),
            "new should accept TS follow flags: {new_cli:?}"
        );

        let reply_cli = Cli::try_parse_from([
            "codex-hermes-bridge",
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
        assert!(
            reply_cli.is_ok(),
            "reply should accept TS follow flags: {reply_cli:?}"
        );

        let approve_cli = Cli::try_parse_from([
            "codex-hermes-bridge",
            "approve",
            "thr_1",
            "approve",
            "--follow",
            "--stream",
        ]);
        assert!(
            approve_cli.is_ok(),
            "approve should accept positional decision and follow flags: {approve_cli:?}"
        );

        let fork_cli = Cli::try_parse_from([
            "codex-hermes-bridge",
            "fork",
            "thr_1",
            "--message",
            "try again",
            "--follow",
            "--experimental-realtime",
        ]);
        assert!(
            fork_cli.is_ok(),
            "fork should accept TS follow flags: {fork_cli:?}"
        );
    }

    #[test]
    fn cli_accepts_ts_archive_filter_flags() {
        let cli = Cli::try_parse_from([
            "codex-hermes-bridge",
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
        assert!(
            cli.is_ok(),
            "archive should accept TS filter flags: {cli:?}"
        );
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
            get_setting_text(&conn, "away")
                .expect("away setting")
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            get_setting_text(&conn, "away_mode").expect("legacy away setting"),
            None
        );
    }

    #[test]
    fn create_state_db_migrates_legacy_threads_cache_columns() {
        let path =
            std::env::temp_dir().join(format!("codex-state-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).expect("open legacy db");
            conn.execute_batch(
                "
                CREATE TABLE threads_cache (
                    thread_id TEXT PRIMARY KEY,
                    name TEXT,
                    cwd TEXT,
                    source TEXT,
                    status_type TEXT NOT NULL,
                    status_flags_json TEXT NOT NULL,
                    updated_at INTEGER,
                    last_seen_at INTEGER NOT NULL
                );
                ",
            )
            .expect("create legacy table");
        }

        let conn = create_state_db(&path).expect("migrated db");
        let columns = table_columns(&conn, "threads_cache").expect("columns");
        assert!(columns.contains(&"last_turn_status".to_string()));
        assert!(columns.contains(&"last_preview".to_string()));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn doctor_realtime_classifies_app_server_failures() {
        let payload = doctor_realtime_result(
            "thr_missing",
            true,
            Err(anyhow!("thread/read: thread not loaded: thr_missing")),
            Err(anyhow!(
                "thread/read: includeTurns is unavailable before first user message"
            )),
            Some(Err(anyhow!(
                "thread/realtime/start: requires experimentalApi capability"
            ))),
            Some(Err(anyhow!(
                "thread/realtime/stop: no rollout found for thread id"
            ))),
        );

        assert_eq!(
            payload["checks"]["readWithTurns"]["error"]["code"],
            "thread_not_loaded"
        );
        assert_eq!(
            payload["checks"]["readWithoutTurns"]["error"]["code"],
            "thread_not_materialized"
        );
        assert_eq!(
            payload["checks"]["realtimeStart"]["error"]["code"],
            "experimental_api_required"
        );
        assert_eq!(
            payload["checks"]["realtimeStop"]["error"]["code"],
            "thread_no_rollout"
        );
    }

    #[test]
    fn app_server_initialize_params_negotiates_experimental_api() {
        let stable = initialize_params(false);
        assert_eq!(stable["capabilities"], json!({}));

        let experimental = initialize_params(true);
        assert_eq!(experimental["capabilities"]["experimentalApi"], true);
        assert_eq!(experimental["clientInfo"]["name"], "codex-hermes-bridge");
    }

    fn env_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    #[cfg(unix)]
    #[test]
    fn resolve_codex_binary_prefers_platform_binary_before_path() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_test_lock().lock().expect("env lock");
        let root = std::env::temp_dir().join(format!("codex-resolve-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        std::fs::create_dir_all(root.join("path-bin")).expect("mkdir path bin");
        std::fs::create_dir_all(home.join("Applications/Codex.app/Contents/Resources"))
            .expect("mkdir platform bin");
        let bad_override = root.join("bad-codex");
        let path_codex = root.join("path-bin/codex");
        let platform_codex = home.join("Applications/Codex.app/Contents/Resources/codex");
        std::fs::write(
            &bad_override,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo bad; exit 42; fi\nexit 42\n",
        )
        .expect("write bad override");
        std::fs::write(
            &path_codex,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo codex-path-1.0; exit 0; fi\nexit 1\n",
        )
        .expect("write path codex");
        std::fs::write(
            &platform_codex,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo codex-platform-1.0; exit 0; fi\nexit 1\n",
        )
        .expect("write platform codex");
        for path in [&bad_override, &path_codex, &platform_codex] {
            let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod");
        }

        let previous_codex_bin = std::env::var("CODEX_BIN").ok();
        let previous_path = std::env::var("PATH").ok();
        let previous_home = std::env::var("HOME").ok();
        std::env::set_var("CODEX_BIN", &bad_override);
        std::env::set_var("HOME", &home);
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                root.join("path-bin").display(),
                previous_path.clone().unwrap_or_default()
            ),
        );

        let resolved = resolve_codex_binary().expect("resolve codex");

        if let Some(previous) = previous_codex_bin {
            std::env::set_var("CODEX_BIN", previous);
        } else {
            std::env::remove_var("CODEX_BIN");
        }
        if let Some(previous) = previous_path {
            std::env::set_var("PATH", previous);
        } else {
            std::env::remove_var("PATH");
        }
        if let Some(previous) = previous_home {
            std::env::set_var("HOME", previous);
        } else {
            std::env::remove_var("HOME");
        }

        assert_eq!(resolved.path, platform_codex);
        assert_eq!(resolved.source, "platform-known");
    }

    #[cfg(unix)]
    #[test]
    fn codex_candidate_probe_times_out() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("codex-probe-timeout-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir timeout root");
        let codex_path = root.join("codex");
        std::fs::write(
            &codex_path,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then sleep 10; exit 0; fi\nexit 1\n",
        )
        .expect("write hanging codex");
        let mut permissions = std::fs::metadata(&codex_path)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&codex_path, permissions).expect("chmod");

        let started = std::time::Instant::now();
        let usable =
            codex_candidate_is_usable_with_timeout(&codex_path, Duration::from_millis(100));

        assert!(!usable);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn app_server_message_parser_preserves_notifications() {
        let mut notifications = Vec::new();
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "item/completed",
            "params": { "threadId": "thr_1" }
        });
        let parsed = handle_app_server_message(&notification, 7, &mut notifications)
            .expect("notification parse");
        assert!(parsed.is_none());
        assert_eq!(notifications, vec![notification]);

        let response = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": { "ok": true }
        });
        let parsed =
            handle_app_server_message(&response, 7, &mut notifications).expect("response parse");
        assert_eq!(parsed, Some(json!({ "ok": true })));
    }

    #[cfg(unix)]
    #[test]
    fn app_server_client_collects_notifications_between_requests() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = env_test_lock().lock().expect("env lock");
        let root = std::env::temp_dir().join(format!(
            "codex-async-notification-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir fake codex root");
        let codex_path = root.join("codex");
        std::fs::write(
            &codex_path,
            r#"#!/usr/bin/env python3
import json
import sys
import time

if len(sys.argv) > 1 and sys.argv[1] == "--version":
    print("codex-fake-async")
    raise SystemExit(0)

if len(sys.argv) > 1 and sys.argv[1] == "app-server":
    line = sys.stdin.readline()
    request = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": {"ok": True}}), flush=True)
    time.sleep(0.05)
    print(json.dumps({
        "jsonrpc": "2.0",
        "method": "item/completed",
        "params": {
            "threadId": "thr_async",
            "turnId": "turn_async",
            "item": {"type": "assistantMessage"}
        }
    }), flush=True)
    time.sleep(1)
    raise SystemExit(0)

raise SystemExit(1)
"#,
        )
        .expect("write fake codex");
        let mut permissions = std::fs::metadata(&codex_path)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&codex_path, permissions).expect("chmod fake codex");

        let previous_codex_bin = std::env::var("CODEX_BIN").ok();
        std::env::set_var("CODEX_BIN", &codex_path);
        let mut client = CodexAppServerClient::connect().expect("connect fake codex");
        if let Some(previous) = previous_codex_bin {
            std::env::set_var("CODEX_BIN", previous);
        } else {
            std::env::remove_var("CODEX_BIN");
        }
        let mut notifications: Vec<Value> = Vec::new();
        for _ in 0..20 {
            notifications.extend(client.drain_notifications());
            if notifications.iter().any(|notification| {
                notification.get("method").and_then(Value::as_str) == Some("item/completed")
            }) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        assert!(notifications.iter().any(|notification| {
            notification.get("method").and_then(Value::as_str) == Some("item/completed")
                && notification
                    .pointer("/params/threadId")
                    .and_then(Value::as_str)
                    == Some("thr_async")
        }));
    }

    #[test]
    fn normalize_notification_matches_ts_event_contract() {
        let event = normalize_notification_event(&json!({
            "jsonrpc": "2.0",
            "method": "item/completed",
            "params": {
                "threadId": "thr_1",
                "turnId": "turn_1",
                "item": { "type": "agentMessage" }
            }
        }));
        assert_eq!(event["type"], "item_completed");
        assert_eq!(event["threadId"], "thr_1");
        assert_eq!(event["turnId"], "turn_1");

        let unknown = normalize_notification_event(&json!({
            "method": "codex/custom",
            "params": { "ok": true }
        }));
        assert_eq!(unknown["type"], "notification");
        assert_eq!(unknown["raw"]["method"], "codex/custom");
    }

    #[test]
    fn follow_events_filter_normalized_notifications() {
        let events = build_follow_events(FollowEventsFixture {
            thread_id: "thr_follow",
            duration_ms: 1000,
            poll_interval_ms: 500,
            experimental_realtime: false,
            initial_thread: Some(json!({"id": "thr_follow"})),
            started: None,
            notifications: vec![json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thr_follow",
                    "turnId": "turn_1",
                    "item": { "type": "agentMessage" }
                }
            })],
            event_filter: Some(&BTreeSet::from(["item_completed".to_string()])),
        })
        .expect("follow events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "item_completed");
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
    fn inbox_items_include_recent_actions() {
        let conn = create_state_db_in_memory().expect("db");
        let reply = snapshot_fixture(
            "thr_reply",
            "/tmp/project-a",
            1000,
            "active",
            vec!["waitingOnUserInput"],
            Some("in_progress"),
        );
        upsert_thread_snapshot(&conn, &reply, 2000).expect("upsert reply");
        record_action(
            &conn,
            "thr_reply",
            "reply",
            json!({"message": "On it"}),
            2100,
        )
        .expect("record action");

        let inbox = list_inbox_from_db(&conn, 3000, None, None, None, None, 10).expect("inbox");
        assert_eq!(
            inbox.items[0].recent_action.as_ref().unwrap()["actionType"],
            "reply"
        );
        assert_eq!(
            inbox.items[0].recent_action.as_ref().unwrap()["payload"]["message"],
            "On it"
        );
    }

    #[test]
    fn show_thread_result_includes_derived_state_and_recent_actions() {
        let conn = create_state_db_in_memory().expect("db");
        record_action(
            &conn,
            "thr_show",
            "approve",
            json!({"decision": "approve"}),
            2200,
        )
        .expect("record action");
        let result = build_show_thread_result(
            Some(&conn),
            "thr_show",
            json!({
                "thread": {
                    "id": "thr_show",
                    "name": null,
                    "cwd": "/tmp/project-a",
                    "status": { "type": "active", "activeFlags": ["waitingOnUserInput"] },
                    "turns": [
                        {
                            "status": "completed",
                            "items": [
                                { "type": "userMessage", "text": "Can you check this?" },
                                { "type": "agentMessage", "text": "I checked it." }
                            ]
                        },
                        {
                            "status": "interrupted",
                            "items": [
                                { "type": "userMessage", "text": "Which option should I pick?" }
                            ]
                        }
                    ]
                }
            }),
        )
        .expect("show result");

        assert_eq!(result["thread"]["threadId"], "thr_show");
        assert_eq!(
            result["thread"]["displayName"],
            "Which option should I pick?"
        );
        assert_eq!(result["thread"]["project"], "project-a");
        assert_eq!(result["thread"]["pendingPrompt"]["kind"], "reply");
        assert_eq!(result["thread"]["canReply"], true);
        assert_eq!(result["thread"]["canApprove"], false);
        assert_eq!(
            result["thread"]["lastUserMessage"],
            "Which option should I pick?"
        );
        assert_eq!(
            result["thread"]["latestQuestion"],
            "Which option should I pick?"
        );
        assert!(result["thread"]["deltaSummary"]
            .as_str()
            .unwrap()
            .contains("Needs input"));
        assert_eq!(
            result["thread"]["recentActions"][0]["actionType"],
            "approve"
        );
    }

    #[test]
    fn reconcile_snapshots_returns_ts_sync_shape_and_dedupes_away_events() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 900).expect("away on");
        let waiting = snapshot_fixture(
            "thr_wait",
            "/tmp/project-a",
            1000,
            "active",
            vec!["waitingOnUserInput"],
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

        let first =
            reconcile_thread_snapshots(&conn, 1200, vec![waiting.clone(), completed.clone()], true)
                .expect("first reconcile");
        assert_eq!(first["synced"], 2);
        assert_eq!(first["away"], true);
        assert_eq!(first["threads"][0]["threadId"], "thr_wait");
        assert!(first["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "thread_waiting"));
        assert!(first["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "thread_completed"));

        let second = reconcile_thread_snapshots(&conn, 1300, vec![waiting, completed], true)
            .expect("second reconcile");
        assert_eq!(second["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn watch_events_from_sync_result_enriches_events_and_notifications() {
        let sync_result = json!({
            "threads": [{
                "threadId": "thr_wait",
                "name": "Need reply",
                "cwd": "/tmp/project",
                "updatedAt": 1000,
                "statusType": "active",
                "statusFlags": ["waitingOnUserInput"],
                "lastTurnStatus": "in_progress",
                "lastPreview": "Question",
                "pendingPrompt": {
                    "promptId": "reply:thr_wait",
                    "promptKind": "reply",
                    "promptStatus": "Needs input",
                    "question": "Question"
                }
            }],
            "events": [{
                "type": "thread_waiting",
                "threadId": "thr_wait",
                "promptKind": "reply",
                "updatedAt": 1000
            }]
        });
        let events = watch_events_from_sync_result(
            &sync_result,
            vec![json!({
                "method": "item/completed",
                "params": {
                    "threadId": "thr_wait",
                    "turnId": "turn_1",
                    "item": { "type": "agentMessage" }
                }
            })],
            None,
        );
        assert_eq!(events[0]["thread"]["pendingPrompt"]["promptKind"], "reply");
        assert!(events
            .iter()
            .any(|event| event["type"] == "item_completed"
                && event["thread"]["threadId"] == "thr_wait"));
    }

    #[test]
    fn notify_away_skips_old_waiting_and_includes_recent_action() {
        let conn = create_state_db_in_memory().expect("db");
        let old_waiting = snapshot_fixture(
            "thr_old_wait",
            "/tmp/project-old",
            500,
            "active",
            vec!["waitingOnUserInput"],
            Some("in_progress"),
        );
        let completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-done",
            1200,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        upsert_thread_snapshot(&conn, &old_waiting, 1300).expect("old waiting");
        upsert_thread_snapshot(&conn, &completed, 1300).expect("completed");
        record_action(&conn, "thr_done", "reply", json!({"message": "done"}), 1250)
            .expect("record action");
        set_away_mode(&conn, true, 1000).expect("away on");

        let notify = build_notify_away_from_db(&conn, 1400, true, true).expect("notify");
        let notifications = notify["notifications"].as_array().unwrap();
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0]["threadId"], "thr_done");
        assert!(notifications[0]["text"]
            .as_str()
            .unwrap()
            .contains("replied"));
    }

    #[test]
    fn notify_away_cli_includes_completed_threads_by_default() {
        let cli = Cli::parse_from(["codex-hermes-bridge", "notify-away"]);

        match cli.command {
            Commands::NotifyAway { completed, .. } => assert!(completed),
            _ => panic!("expected notify-away command"),
        }
    }

    #[test]
    fn hermes_hook_notifies_on_thread_completed_event() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/hermes-codex-hook.py");
        let mut child = Command::new("python3")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn hermes hook");

        let payload = json!({
            "type": "thread_completed",
            "threadId": "thr_done",
            "thread": {
                "name": "Launch checklist",
                "lastPreview": "Finished the launch checklist."
            }
        });
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(serde_json::to_string(&payload).unwrap().as_bytes())
            .expect("write payload");
        drop(child.stdin.take());

        let output = child.wait_with_output().expect("hook output");
        assert!(output.status.success());
        let notification: Value =
            serde_json::from_slice(&output.stdout).expect("hook should emit JSON");

        assert_eq!(notification["notify"], true);
        assert_eq!(notification["threadId"], "thr_done");
        assert_eq!(notification["eventType"], "thread_completed");
        assert_eq!(notification["message"], "Finished the launch checklist.");
    }

    #[test]
    fn notify_away_does_not_drop_completed_threads_behind_active_inbox_limit() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 1000).expect("away on");
        for idx in 0..100 {
            let active = snapshot_fixture(
                &format!("thr_active_{idx}"),
                "/tmp/project-active",
                3000 + idx,
                "active",
                vec![],
                Some("in_progress"),
            );
            upsert_thread_snapshot(&conn, &active, 4000).expect("upsert active");
        }
        let completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-done",
            2000,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        upsert_thread_snapshot(&conn, &completed, 4000).expect("upsert completed");

        let notify = build_notify_away_from_db(&conn, 5000, true, true).expect("notify");
        let notifications = notify["notifications"].as_array().unwrap();

        assert!(notifications.iter().any(|notification| {
            notification["type"] == "thread_completed" && notification["threadId"] == "thr_done"
        }));
    }

    #[test]
    fn notify_away_redelivers_completed_thread_when_summary_changes_without_timestamp_change() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 1000).expect("away on");
        let mut completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-done",
            2000,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        completed.last_preview = Some("First summary".to_string());
        upsert_thread_snapshot(&conn, &completed, 3000).expect("upsert first");

        let first = build_notify_away_from_db(&conn, 4000, true, true).expect("first notify");
        assert_eq!(first["notifications"].as_array().unwrap().len(), 1);

        completed.last_preview = Some("Second summary".to_string());
        upsert_thread_snapshot(&conn, &completed, 5000).expect("upsert second");

        let second = build_notify_away_from_db(&conn, 6000, true, true).expect("second notify");
        let notifications = second["notifications"].as_array().unwrap();
        assert_eq!(notifications.len(), 1);
        assert!(notifications[0]["text"]
            .as_str()
            .unwrap()
            .contains("Second summary"));
    }

    #[test]
    fn notify_away_preserves_completed_summary_body_beyond_inbox_preview_length() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 1000).expect("away on");
        let mut completed = snapshot_fixture(
            "thr_done",
            "/tmp/project-done",
            2000,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        completed.last_preview = Some(format!(
            "{}\n\n{}",
            "A".repeat(260),
            "The important final line survives."
        ));
        upsert_thread_snapshot(&conn, &completed, 3000).expect("upsert completed");

        let notify = build_notify_away_from_db(&conn, 4000, true, true).expect("notify");
        let text = notify["notifications"][0]["text"].as_str().unwrap();

        assert!(text.contains("The important final line survives."));
    }

    #[test]
    fn notify_away_completed_survives_older_sync_timestamp() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 1_776_047_824_152).expect("away on");
        let cached = snapshot_fixture(
            "thr_seconds",
            "/tmp/project-seconds",
            1_776_047_900_000,
            "notLoaded",
            vec![],
            Some("completed"),
        );
        upsert_thread_snapshot(&conn, &cached, 1_776_048_898_482).expect("upsert cached");
        let older_sync = snapshot_fixture(
            "thr_seconds",
            "/tmp/project-seconds",
            1_776_047_790,
            "notLoaded",
            vec![],
            Some("completed"),
        );

        reconcile_thread_snapshots(&conn, 500, vec![older_sync], true).expect("reconcile");
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at FROM threads_cache WHERE thread_id = ?1",
                params!["thr_seconds"],
                |row| row.get(0),
            )
            .expect("updated_at");
        assert_eq!(updated_at, 1_776_047_900_000);

        let notify = build_notify_away_from_db(&conn, 600, true, true).expect("notify");
        let notifications = notify["notifications"].as_array().unwrap();
        assert!(notifications.iter().any(|notification| {
            notification["type"] == "thread_completed" && notification["threadId"] == "thr_seconds"
        }));
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

        let first = build_notify_away_from_db(&conn, 2500, true, true).expect("first notify");
        assert_eq!(first["action"], "notify-away");
        assert_eq!(first["notifications"].as_array().unwrap().len(), 1);
        assert!(first["notifications"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Tell me how you want me to reply"));

        let second = build_notify_away_from_db(&conn, 2600, true, true).expect("second notify");
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
        let rendered = render_notification_message(NotificationMessage {
            kind: "thread_completed",
            display_name: "Sample thread",
            project: "project-a",
            cwd: Some("/tmp/project-a"),
            summary: Some("Finished work"),
            detail: None,
            recent_action: Some("Last action: replied"),
            next_step: Some("Tell me if you want me to follow up"),
        });
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

        let dry_run =
            unarchive_thread_result(&conn, "thr_archived", true, 1000, None).expect("dry run");
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
        assert_eq!(
            audit["audits"][0]["derived"]["attentionReason"],
            "pending_approval"
        );
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
        let events = build_follow_events(FollowEventsFixture {
            thread_id: "thr_follow",
            duration_ms: 1000,
            poll_interval_ms: 500,
            experimental_realtime: false,
            initial_thread: Some(thread.clone()),
            started: None,
            notifications: vec![],
            event_filter: None,
        })
        .expect("follow events");
        assert_eq!(events[0]["type"], "follow_started");
        assert_eq!(events[1]["type"], "follow_snapshot");
        assert_eq!(events[1]["thread"], thread);
    }

    #[test]
    fn follow_summaries_match_direct_and_composed_ts_shapes() {
        let events = vec![json!({"type": "follow_started", "threadId": "thr_follow"})];

        let direct = follow_result_summary("thr_follow", 3000, false, &events, false);
        assert_eq!(direct["ok"], true);
        assert_eq!(direct["action"], "follow");
        assert_eq!(direct["threadId"], "thr_follow");
        assert!(direct.get("events").is_none());

        let composed = follow_result_summary("thr_follow", 3000, false, &events, true);
        assert_eq!(composed["events"].as_array().unwrap().len(), 1);

        let command = attach_follow_payload(
            json!({"ok": true, "action": "reply", "threadId": "thr_follow"}),
            direct,
            true,
        );
        assert_eq!(command["type"], "command_result");
        assert_eq!(command["result"]["follow"]["action"], "follow");
    }

    #[test]
    fn composed_follow_settle_duration_matches_ts_default() {
        assert_eq!(composed_settle_duration_ms(100), 0);
        assert_eq!(composed_settle_duration_ms(101), 20_000);
        assert_eq!(composed_settle_duration_ms(3_000), 20_000);
    }

    #[test]
    fn new_thread_request_and_result_shapes_match_ts() {
        assert_eq!(thread_start_params(None, None), json!({}));
        assert_eq!(
            thread_start_params(Some("/tmp/project"), Some("hello")),
            json!({
                "cwd": "/tmp/project",
                "input": [{"type": "text", "text": "hello", "text_elements": []}]
            })
        );
        assert_eq!(
            turn_start_params("thr_new", Some("/tmp/project"), "hello"),
            json!({
                "threadId": "thr_new",
                "cwd": "/tmp/project",
                "input": [{"type": "text", "text": "hello", "text_elements": []}]
            })
        );

        let result = new_thread_live_result(
            Some("/tmp/input"),
            Some(" hello "),
            json!({"thread": {"id": "thr_new", "cwd": "/tmp/from-codex"}}),
            Some(json!({"turn": {"id": "turn_1"}})),
        );
        assert_eq!(result["threadId"], "thr_new");
        assert_eq!(result["cwd"], "/tmp/from-codex");
        assert_eq!(result["message"], "hello");
        assert_eq!(result["started"]["turn"]["id"], "turn_1");
    }

    #[test]
    fn archive_targets_match_ts_explicit_and_filtered_selection() {
        let conn = create_state_db_in_memory().expect("db");
        let project_a = snapshot_fixture(
            "thr_a",
            "/tmp/project-a",
            1000,
            "active",
            vec!["waitingOnUserInput"],
            Some("in_progress"),
        );
        let project_b = snapshot_fixture(
            "thr_b",
            "/tmp/project-b",
            900,
            "idle",
            vec![],
            Some("completed"),
        );
        upsert_thread_snapshot(&conn, &project_a, 1000).expect("upsert a");
        upsert_thread_snapshot(&conn, &project_b, 1000).expect("upsert b");

        let explicit = resolve_archive_targets(
            &conn,
            &["thr_a".into(), "thr_b".into()],
            None,
            None,
            None,
            100,
            1500,
        )
        .expect("explicit targets");
        assert_eq!(explicit.targets, vec!["thr_a", "thr_b"]);
        assert!(!explicit.using_filter_selection);

        let filtered = resolve_archive_targets(
            &conn,
            &[],
            Some("project-a"),
            Some("active"),
            None,
            10,
            1500,
        )
        .expect("filtered targets");
        assert_eq!(filtered.targets, vec!["thr_a"]);
        assert!(filtered.using_filter_selection);

        let implicit_bulk = resolve_archive_targets(&conn, &[], None, None, None, 10, 1500)
            .expect("implicit bulk targets");
        assert!(implicit_bulk.using_filter_selection);
        let bulk_without_yes =
            archive_from_db(&conn, None, None, false, false, 1500).expect_err("bulk refusal");
        assert!(format!("{bulk_without_yes:#}").contains("Refusing bulk archive"));
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
            Some(&std::collections::BTreeSet::from([
                "item_completed".to_string()
            ])),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["type"], "item_completed");
    }

    #[test]
    fn watch_thread_error_event_matches_ts_stream_shape() {
        let event = watch_thread_error_event(&anyhow!("sync failed"));
        assert_eq!(event["type"], "thread_error");
        assert_eq!(event["message"], "sync failed");

        let filtered = filter_watch_events(
            vec![event],
            Some(&BTreeSet::from(["thread_waiting".to_string()])),
        );
        assert!(filtered.is_empty());
    }

    #[test]
    fn watch_exec_hook_writes_event_to_stdout_consumer() {
        let temp = std::env::temp_dir().join(format!("codex-watch-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).expect("mkdir temp");
        let hook_output = temp.join("hook.json");
        let command = format!(
            "python3 -c \"import sys,pathlib; pathlib.Path(r'{}').write_text(sys.stdin.read())\"",
            hook_output.display()
        );
        run_exec_hook(
            &command,
            &json!({"type": "thread_waiting", "threadId": "thr_1"}),
        )
        .expect("exec hook");
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
