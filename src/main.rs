use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use notify::Watcher as _;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(name = "codex-hermes-bridge")]
#[command(version)]
#[command(about = "Inspect Codex threads and stream normalized automation events")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "Inspect Codex setup")]
    Doctor,
    #[command(about = "Manage away-mode notification state")]
    Away {
        #[command(subcommand)]
        command: AwayCommands,
    },
    #[command(about = "List recent Codex threads")]
    Threads {
        #[arg(long, default_value_t = 25)]
        limit: u64,
    },
    #[command(about = "Stream thread updates")]
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
    },
    #[command(about = "Unarchive a Codex thread")]
    Unarchive {
        thread_id: String,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    #[command(about = "List threads waiting for user attention")]
    Waiting {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: u64,
    },
    #[command(about = "List actionable thread inbox rows")]
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
    #[command(about = "Watch for thread changes and emit JSON events")]
    Watch {
        #[arg(long, default_value_t = false)]
        once: bool,
        #[arg(long)]
        exec: Option<String>,
        #[arg(long)]
        events: Option<String>,
    },
    #[command(about = "Emit away-mode notification summaries")]
    NotifyAway {
        #[arg(long = "no-completed", default_value_t = true, action = clap::ArgAction::SetFalse)]
        completed: bool,
        #[arg(long, default_value_t = false)]
        mark_delivered: bool,
    },
    #[command(about = "Sync live Codex thread state into the local cache")]
    Sync {
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    #[command(about = "Start a new Codex thread")]
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
        prompt: Vec<String>,
    },
    #[command(about = "Fork a Codex thread")]
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
        prompt: Vec<String>,
    },
    #[command(about = "Archive explicit threads or selected inbox rows")]
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
    #[command(about = "Show a thread with derived bridge state")]
    Show { thread_id: String },
    #[command(about = "Reply to a waiting Codex thread")]
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
        prompt: Vec<String>,
    },
    #[command(about = "Approve or deny a waiting approval prompt")]
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
        positional_decision: Option<String>,
    },
    #[command(about = "Run a stdio MCP server for Hermes")]
    Mcp,
    #[command(about = "Install or inspect Hermes integration helpers")]
    Hermes {
        #[command(subcommand)]
        command: HermesCommands,
    },
}

#[derive(Subcommand, Debug)]
enum AwayCommands {
    On,
    Off,
    Status,
}

#[derive(Subcommand, Debug)]
enum HermesCommands {
    #[command(about = "Register this bridge with Hermes MCP and optional notification delivery")]
    Install {
        #[arg(long, default_value = "codex")]
        server_name: String,
        #[arg(long, default_value = "hermes")]
        hermes_command: String,
        #[arg(long, default_value = "codex-hermes-bridge")]
        bridge_command: String,
        #[arg(long, default_value = "codex-watch")]
        webhook_name: String,
        #[arg(long, default_value = DEFAULT_HERMES_WEBHOOK_EVENTS)]
        webhook_events: String,
        #[arg(long)]
        webhook_deliver: Option<String>,
        #[arg(long)]
        webhook_deliver_chat_id: Option<String>,
        #[arg(long)]
        webhook_secret: Option<String>,
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    #[command(about = "Post one watch event to a signed Hermes webhook")]
    PostWebhook {
        #[arg(long)]
        url: String,
        #[arg(long)]
        secret: String,
        #[arg(long, default_value_t = 10000)]
        timeout_ms: u64,
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
        Commands::Doctor => {
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
            let mut client = CodexAppServerClient::connect()?;
            let result = sync_state_from_live(&mut client, &conn, now, limit, false)?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "threads": result["threads"].clone()
                }))?
            );
        }
        Commands::Follow {
            thread_id,
            message,
            duration,
            poll_interval,
            events,
        } => {
            let event_filter = parse_event_filter(events.as_deref());
            let mut client = CodexAppServerClient::connect()?;
            let events = collect_follow_events(
                &mut client,
                &thread_id,
                message.as_deref(),
                duration,
                poll_interval,
                event_filter.as_ref(),
            )?;
            for event in &events {
                println!("{}", serde_json::to_string(event)?);
            }
            println!(
                "{}",
                serde_json::to_string(&follow_result_summary(
                    &thread_id, duration, &events, false,
                ))?
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
        Commands::Waiting { project, limit } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
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
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
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
                let mut client = CodexAppServerClient::connect()?;
                let sync_result = match sync_state_from_live(&mut client, &conn, now, 50, true) {
                    Ok(sync_result) => sync_result,
                    Err(error) => {
                        let filtered = filter_watch_events(
                            vec![watch_thread_error_event(&error)],
                            filter.as_ref(),
                        );
                        for event in &filtered {
                            if let Some(command) = exec.as_deref() {
                                run_exec_hook(command, event)?;
                            }
                        }
                        println!("{}", serde_json::to_string(&json!({ "events": filtered }))?);
                        return Ok(());
                    }
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
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, 50, true)?;
            let result = build_notify_away_from_db(&conn, now, completed, mark_delivered)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Sync { limit } => {
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            let result = sync_state_from_live(&mut client, &conn, now, limit, false)?;
            println!("{}", serde_json::to_string(&result)?);
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
                let mut client = CodexAppServerClient::connect()?;
                let created =
                    client.request("thread/start", thread_start_params(cwd.as_deref()))?;
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
                let mut client = CodexAppServerClient::connect()?;
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
            if !dry_run && targets.is_empty() && !yes {
                bail!("Refusing bulk archive without --yes or --dry-run");
            }
            let mut client = if dry_run {
                None
            } else {
                Some(CodexAppServerClient::connect()?)
            };
            if !dry_run && targets.is_empty() {
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
                serde_json::to_string(
                    &build_show_thread_result(Some(&conn), &thread_id, result,)?
                )?
            );
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
        Commands::Mcp => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            run_mcp_server(stdin.lock(), stdout.lock())?;
        }
        Commands::Hermes { command } => match command {
            HermesCommands::Install {
                server_name,
                hermes_command,
                bridge_command,
                webhook_name,
                webhook_events,
                webhook_deliver,
                webhook_deliver_chat_id,
                webhook_secret,
                dry_run,
            } => {
                let result = run_hermes_install(HermesInstallOptions {
                    server_name: &server_name,
                    hermes_command: &hermes_command,
                    bridge_command: &bridge_command,
                    webhook_name: &webhook_name,
                    webhook_events: &webhook_events,
                    webhook_deliver: webhook_deliver.as_deref(),
                    webhook_deliver_chat_id: webhook_deliver_chat_id.as_deref(),
                    webhook_secret: webhook_secret.as_deref(),
                    dry_run,
                })?;
                println!("{}", serde_json::to_string(&result)?);
            }
            HermesCommands::PostWebhook {
                url,
                secret,
                timeout_ms,
            } => {
                let mut input = String::new();
                std::io::stdin().read_to_string(&mut input)?;
                let event: Value = serde_json::from_str(input.trim())
                    .context("failed to parse watch event JSON from stdin")?;
                let result =
                    post_hermes_webhook(&url, &secret, &event, Duration::from_millis(timeout_ms))?;
                println!("{}", serde_json::to_string(&result)?);
            }
        },
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
    ensure_column(
        conn,
        "threads_cache",
        "last_seen_at",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
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

fn thread_start_params(cwd: Option<&str>) -> Value {
    let mut params = serde_json::Map::new();
    if let Some(cwd) = cwd.filter(|value| !value.trim().is_empty()) {
        params.insert("cwd".to_string(), json!(cwd));
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

    json!({
        "code": "app_server_error",
        "retryable": false,
        "message": message,
        "guidance": Value::Null
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
            "pollIntervalMs": poll_interval_ms
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
    event_filter: Option<&'a BTreeSet<String>>,
    stream: bool,
}

fn follow_result_summary(
    thread_id: &str,
    duration_ms: u64,
    events: &[Value],
    include_events: bool,
) -> Value {
    let mut summary = json!({
        "ok": true,
        "action": "follow",
        "threadId": thread_id,
        "durationMs": duration_ms
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
        follow.event_filter,
    )?;
    if follow.stream {
        for event in &events {
            println!("{}", serde_json::to_string(event)?);
        }
        let follow_summary =
            follow_result_summary(follow.thread_id, follow.duration_ms, &events, false);
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
    let follow_summary = follow_result_summary(follow.thread_id, follow.duration_ms, &events, true);
    Ok(attach_follow_payload(result, follow_summary, false))
}

#[cfg(test)]
struct FollowEventsFixture<'a> {
    thread_id: &'a str,
    duration_ms: u64,
    poll_interval_ms: u64,
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
        "pollIntervalMs": fixture.poll_interval_ms
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
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn exec hook: {command}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(serde_json::to_string(event)?.as_bytes())?;
    }
    let status = child
        .wait()
        .with_context(|| format!("failed to wait for exec hook: {command}"))?;
    if !status.success() {
        bail!("exec hook `{command}` exited with status {status}");
    }
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
        let summary = payload
            .get("lastPreview")
            .and_then(Value::as_str)
            .map(str::to_string);
        let exists = candidates
            .iter()
            .any(|candidate| candidate.delivery_key == event_key);
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
                recent_action: recent_action_label(
                    recent_actions_json(conn, &thread_id, 1)?.first(),
                )
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

const DEFAULT_HERMES_WEBHOOK_EVENTS: &str = "thread_waiting,thread_completed,thread_status_changed";

const HERMES_WEBHOOK_PROMPT: &str = r#"Codex changed without you asking.

Use the event payload to send Hanif a concise notification. If the event says Codex needs a reply or approval, ask for the exact reply or decision. If Hanif responds, use the Codex MCP tools for this same thread_id.

If reply_route_marker is present in the payload, include that exact value as the final line so Hermes can route a platform reply back to the Codex thread.

Event payload:
{__raw__}"#;

#[derive(Debug, Clone, Copy)]
struct HermesInstallOptions<'a> {
    server_name: &'a str,
    hermes_command: &'a str,
    bridge_command: &'a str,
    webhook_name: &'a str,
    webhook_events: &'a str,
    webhook_deliver: Option<&'a str>,
    webhook_deliver_chat_id: Option<&'a str>,
    webhook_secret: Option<&'a str>,
    dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HermesWebhookSubscription {
    url: Option<String>,
    secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpTarget {
    host: String,
    port: u16,
    path: String,
    host_header: String,
}

fn run_hermes_install(options: HermesInstallOptions<'_>) -> Result<Value> {
    let server_name = options.server_name.trim();
    let hermes_command = options.hermes_command.trim();
    let bridge_command = options.bridge_command.trim();
    let webhook_name = options.webhook_name.trim();
    let webhook_events = options.webhook_events.trim();

    if server_name.is_empty() {
        bail!("server name cannot be empty");
    }
    if hermes_command.is_empty() {
        bail!("hermes command cannot be empty");
    }
    if bridge_command.is_empty() {
        bail!("bridge command cannot be empty");
    }
    if webhook_name.is_empty() {
        bail!("webhook name cannot be empty");
    }
    if webhook_events.is_empty() {
        bail!("webhook events cannot be empty");
    }

    let mcp_args = vec![
        "mcp".to_string(),
        "add".to_string(),
        server_name.to_string(),
        "--command".to_string(),
        bridge_command.to_string(),
        "--args".to_string(),
        "mcp".to_string(),
    ];
    let planned_webhook_url = default_hermes_webhook_url(webhook_name);
    let webhook_secret = options
        .webhook_secret
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let planned_secret = webhook_secret.unwrap_or("<secret printed by hermes webhook subscribe>");
    let planned_watcher_command = hermes_watcher_command(
        bridge_command,
        webhook_events,
        &planned_webhook_url,
        planned_secret,
    );
    let webhook_deliver = options
        .webhook_deliver
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let webhook_deliver_chat_id = options
        .webhook_deliver_chat_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let webhook_args = webhook_deliver.map(|deliver| {
        hermes_webhook_subscribe_args(
            webhook_name,
            webhook_events,
            deliver,
            webhook_deliver_chat_id,
            webhook_secret,
        )
    });

    let base = json!({
        "ok": true,
        "action": "hermes_install",
        "dryRun": options.dry_run,
        "serverName": server_name,
        "hermesCommand": hermes_command,
        "bridgeCommand": bridge_command,
        "args": mcp_args.clone(),
        "mcp": {
            "configured": true,
            "args": mcp_args.clone(),
            "nextStep": "Restart Hermes so it reconnects to MCP servers and discovers codex_* tools."
        },
        "notificationLane": {
            "configured": webhook_args.is_some(),
            "webhookName": webhook_name,
            "events": webhook_events,
            "deliver": webhook_deliver,
            "deliverChatId": webhook_deliver_chat_id,
            "webhookUrl": planned_webhook_url,
            "webhookArgs": webhook_args.clone(),
            "watcherCommand": planned_watcher_command,
            "nextStep": if webhook_args.is_some() {
                "Run watcherCommand as a long-lived local process so Codex changes proactively notify Hermes."
            } else {
                "Pass --webhook-deliver, for example --webhook-deliver telegram, to install proactive Hermes notifications."
            }
        },
        "nextStep": "Restart Hermes for MCP discovery, then run notificationLane.watcherCommand if notificationLane.configured is true."
    });
    if options.dry_run {
        return Ok(base);
    }

    let mcp_output = Command::new(hermes_command)
        .args(&mcp_args)
        .output()
        .with_context(|| format!("failed to run {hermes_command} mcp add"))?;
    if !mcp_output.status.success() {
        bail!(
            "Hermes MCP registration failed with status {}: {}",
            mcp_output.status,
            String::from_utf8_lossy(&mcp_output.stderr).trim()
        );
    }

    let notification_result = if let Some(webhook_args) = webhook_args {
        let output = Command::new(hermes_command)
            .args(&webhook_args)
            .output()
            .with_context(|| format!("failed to run {hermes_command} webhook subscribe"))?;
        if !output.status.success() {
            bail!(
                "Hermes webhook subscription failed with status {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let parsed = parse_hermes_webhook_subscribe_output(&stdout);
        let webhook_url = parsed
            .url
            .clone()
            .unwrap_or_else(|| default_hermes_webhook_url(webhook_name));
        let resolved_secret = webhook_secret
            .map(str::to_string)
            .or(parsed.secret.clone())
            .context("Hermes webhook subscription did not print a secret; pass --webhook-secret")?;
        let watcher_command = hermes_watcher_command(
            bridge_command,
            webhook_events,
            &webhook_url,
            &resolved_secret,
        );
        Some(json!({
            "configured": true,
            "args": webhook_args,
            "stdout": stdout,
            "stderr": String::from_utf8_lossy(&output.stderr).trim(),
            "webhookUrl": webhook_url,
            "watcherCommand": watcher_command,
            "parsed": {
                "url": parsed.url,
                "secret": parsed.secret.map(|_| "<redacted>")
            }
        }))
    } else {
        None
    };

    Ok(json!({
        "ok": true,
        "action": "hermes_install",
        "dryRun": false,
        "serverName": server_name,
        "hermesCommand": hermes_command,
        "bridgeCommand": bridge_command,
        "args": mcp_args,
        "mcp": {
            "configured": true,
            "stdout": String::from_utf8_lossy(&mcp_output.stdout).trim(),
            "stderr": String::from_utf8_lossy(&mcp_output.stderr).trim()
        },
        "notificationLane": notification_result.unwrap_or_else(|| json!({
            "configured": false,
            "nextStep": "Pass --webhook-deliver, for example --webhook-deliver telegram, to install proactive Hermes notifications."
        })),
        "nextStep": "Restart Hermes for MCP discovery, then run notificationLane.watcherCommand if notificationLane.configured is true."
    }))
}

fn hermes_webhook_subscribe_args(
    webhook_name: &str,
    webhook_events: &str,
    deliver: &str,
    deliver_chat_id: Option<&str>,
    secret: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "webhook".to_string(),
        "subscribe".to_string(),
        webhook_name.to_string(),
        "--description".to_string(),
        "Proactive Codex thread notifications from codex-hermes-bridge".to_string(),
        "--events".to_string(),
        webhook_events.to_string(),
        "--prompt".to_string(),
        HERMES_WEBHOOK_PROMPT.to_string(),
        "--deliver".to_string(),
        deliver.to_string(),
    ];
    if let Some(chat_id) = deliver_chat_id {
        args.push("--deliver-chat-id".to_string());
        args.push(chat_id.to_string());
    }
    if let Some(secret) = secret {
        args.push("--secret".to_string());
        args.push(secret.to_string());
    }
    args
}

fn default_hermes_webhook_url(webhook_name: &str) -> String {
    format!("http://localhost:8644/webhooks/{webhook_name}")
}

fn hermes_watcher_command(
    bridge_command: &str,
    events: &str,
    webhook_url: &str,
    webhook_secret: &str,
) -> String {
    let inner = format!(
        "{} hermes post-webhook --url {} --secret {}",
        shell_quote(bridge_command),
        shell_quote(webhook_url),
        shell_quote(webhook_secret)
    );
    format!(
        "{} watch --events {} --exec {}",
        shell_quote(bridge_command),
        shell_quote(events),
        shell_quote(&inner)
    )
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ',' | ':' | '=')
        })
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn parse_hermes_webhook_subscribe_output(output: &str) -> HermesWebhookSubscription {
    let mut subscription = HermesWebhookSubscription {
        url: None,
        secret: None,
    };
    for line in output.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if (lower.starts_with("url:") || lower.starts_with("webhook url:"))
            && subscription.url.is_none()
        {
            if let Some((_, value)) = trimmed.split_once(':') {
                subscription.url = Some(value.trim().to_string());
            }
        } else if lower.starts_with("secret:") && subscription.secret.is_none() {
            if let Some((_, value)) = trimmed.split_once(':') {
                subscription.secret = Some(value.trim().to_string());
            }
        }
    }
    subscription
}

fn post_hermes_webhook(url: &str, secret: &str, event: &Value, timeout: Duration) -> Result<Value> {
    if secret.trim().is_empty() {
        bail!("webhook secret cannot be empty");
    }
    let target = parse_http_url(url)?;
    let payload = hermes_webhook_payload(event);
    let body = serde_json::to_vec(&payload)?;
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or("codex_event")
        .to_string();
    let delivery_id = hermes_webhook_delivery_id(event);
    let signature = hmac_sha256_hex(secret.trim().as_bytes(), &body);
    let mut stream = TcpStream::connect((target.host.as_str(), target.port))
        .with_context(|| format!("failed to connect to Hermes webhook at {url}"))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Hub-Signature-256: sha256={}\r\nX-GitHub-Event: {}\r\nX-GitHub-Delivery: {}\r\nConnection: close\r\n\r\n",
        target.path,
        target.host_header,
        body.len(),
        signature,
        event_type,
        delivery_id
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let status_line = response.lines().next().unwrap_or_default().to_string();
    let status_code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .context("Hermes webhook response did not include an HTTP status")?;
    if !(200..300).contains(&status_code) {
        bail!("Hermes webhook returned {status_line}");
    }

    Ok(json!({
        "ok": true,
        "action": "hermes_post_webhook",
        "eventType": event_type,
        "deliveryId": delivery_id,
        "statusCode": status_code,
        "statusLine": status_line
    }))
}

fn parse_http_url(url: &str) -> Result<HttpTarget> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .context("Hermes webhook URL must use http://")?;
    let (authority, path_suffix) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        bail!("Hermes webhook URL is missing a host");
    }
    if authority.contains('@') {
        bail!("Hermes webhook URL must not include credentials");
    }
    let (host, port, explicit_port) = if authority.starts_with('[') {
        let end = authority
            .find(']')
            .context("IPv6 webhook URL host is missing closing bracket")?;
        let host = authority[1..end].to_string();
        let suffix = &authority[(end + 1)..];
        let port = if let Some(raw) = suffix.strip_prefix(':') {
            raw.parse::<u16>()
                .with_context(|| format!("invalid webhook port: {raw}"))?
        } else if suffix.is_empty() {
            80
        } else {
            bail!("invalid webhook URL authority: {authority}");
        };
        (host, port, port != 80)
    } else if let Some((host, raw_port)) = authority.rsplit_once(':') {
        if host.is_empty() {
            bail!("Hermes webhook URL is missing a host");
        }
        let port = raw_port
            .parse::<u16>()
            .with_context(|| format!("invalid webhook port: {raw_port}"))?;
        (host.to_string(), port, port != 80)
    } else {
        (authority.to_string(), 80, false)
    };
    if host.trim().is_empty() {
        bail!("Hermes webhook URL is missing a host");
    }
    let path = format!("/{}", path_suffix);
    let host_header = if explicit_port {
        if authority.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        }
    } else {
        host.clone()
    };
    Ok(HttpTarget {
        host,
        port,
        path,
        host_header,
    })
}

fn hermes_webhook_payload(event: &Value) -> Value {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("codex_event");
    let thread_id = event_thread_id(event);
    let reply_route = thread_id.as_ref().map(|thread_id| {
        json!({
            "route_kind": "codex_reply",
            "thread_id": thread_id
        })
    });
    json!({
        "event_type": event_type,
        "source": "codex-hermes-bridge",
        "thread_id": thread_id,
        "route_kind": "codex_reply",
        "reply_route": reply_route,
        "reply_route_marker": reply_route.as_ref().map(|route| {
            format!("THREAD_ROUTE {}", serde_json::to_string(route).unwrap_or_default())
        }),
        "event": event
    })
}

fn event_thread_id(event: &Value) -> Option<String> {
    event
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/thread/threadId").and_then(Value::as_str))
        .or_else(|| event.pointer("/thread/id").and_then(Value::as_str))
        .map(str::to_string)
}

fn hermes_webhook_delivery_id(event: &Value) -> String {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("codex_event");
    let thread_id = event_thread_id(event).unwrap_or_else(|| "unknown".to_string());
    let discriminator = event
        .get("eventKey")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            event
                .get("updatedAt")
                .and_then(Value::as_u64)
                .map(|value| value.to_string())
        })
        .or_else(|| {
            event
                .get("observedAt")
                .and_then(Value::as_u64)
                .map(|value| value.to_string())
        })
        .unwrap_or_else(|| {
            serde_json::to_string(event)
                .map(|raw| sha256_hex(raw.as_bytes()))
                .unwrap_or_else(|_| "event".to_string())
        });
    sanitize_delivery_id(&format!("codex:{event_type}:{thread_id}:{discriminator}"))
}

fn sanitize_delivery_id(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    const BLOCK_SIZE: usize = 64;
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let digest = Sha256::digest(key);
        key_block[..digest.len()].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut outer_key_pad = [0u8; BLOCK_SIZE];
    let mut inner_key_pad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        outer_key_pad[i] = key_block[i] ^ 0x5c;
        inner_key_pad[i] = key_block[i] ^ 0x36;
    }

    let mut inner = Sha256::new();
    inner.update(inner_key_pad);
    inner.update(message);
    let inner_hash = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_key_pad);
    outer.update(inner_hash);
    hex_lower(&outer.finalize())
}

fn sha256_hex(message: &[u8]) -> String {
    hex_lower(&Sha256::digest(message))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

fn run_mcp_server<R: BufRead, W: Write>(reader: R, mut writer: W) -> Result<()> {
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed = match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) => parsed,
            Err(error) => {
                let response =
                    mcp_error_response(None, -32700, format!("Parse error: {error}"), Value::Null);
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
                continue;
            }
        };

        if let Some(response) = mcp_handle_message(parsed) {
            writeln!(writer, "{}", serde_json::to_string(&response)?)?;
            writer.flush()?;
        }
    }
    Ok(())
}

fn mcp_handle_message(message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = match message.get("method").and_then(Value::as_str) {
        Some(method) => method,
        None => {
            return id.map(|id| {
                mcp_error_response(
                    Some(id),
                    -32600,
                    "Invalid request: missing method",
                    Value::Null,
                )
            })
        }
    };
    let params = message.get("params").unwrap_or(&Value::Null);

    match method {
        "notifications/initialized" => None,
        "initialize" => id.map(|id| mcp_success_response(id, mcp_initialize_result(params))),
        "ping" => id.map(|id| mcp_success_response(id, json!({}))),
        "tools/list" => id.map(|id| mcp_success_response(id, json!({ "tools": mcp_tools() }))),
        "tools/call" => id.map(|id| {
            let result = match mcp_tool_call_result(params) {
                Ok(result) => result,
                Err(error) => mcp_tool_error_result(&error),
            };
            mcp_success_response(id, result)
        }),
        "resources/list" => {
            id.map(|id| mcp_success_response(id, json!({ "resources": mcp_resources() })))
        }
        "resources/templates/list" => id.map(|id| {
            mcp_success_response(
                id,
                json!({
                    "resourceTemplates": [{
                        "uriTemplate": "codex://thread/{threadId}",
                        "name": "Codex Thread",
                        "title": "Codex Thread",
                        "description": "Read one Codex thread by id.",
                        "mimeType": "application/json"
                    }]
                }),
            )
        }),
        "resources/read" => id.map(|id| match mcp_read_resource(params) {
            Ok(result) => mcp_success_response(id, result),
            Err(error) => mcp_error_response(
                Some(id),
                -32602,
                format!("Invalid resource read: {error:#}"),
                Value::Null,
            ),
        }),
        "prompts/list" => {
            id.map(|id| mcp_success_response(id, json!({ "prompts": mcp_prompts() })))
        }
        "prompts/get" => id.map(|id| match mcp_get_prompt(params) {
            Ok(result) => mcp_success_response(id, result),
            Err(error) => mcp_error_response(
                Some(id),
                -32602,
                format!("Invalid prompt request: {error:#}"),
                Value::Null,
            ),
        }),
        _ => id.map(|id| {
            mcp_error_response(
                Some(id),
                -32601,
                format!("Method not found: {method}"),
                Value::Null,
            )
        }),
    }
}

fn mcp_success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn mcp_error_response(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    data: Value,
) -> Value {
    let mut response = serde_json::Map::new();
    response.insert("jsonrpc".to_string(), json!("2.0"));
    response.insert("id".to_string(), id.unwrap_or(Value::Null));
    response.insert(
        "error".to_string(),
        json!({
            "code": code,
            "message": message.into(),
            "data": data
        }),
    );
    Value::Object(response)
}

fn mcp_initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSION);
    let protocol_version = if requested == MCP_PROTOCOL_VERSION {
        requested
    } else {
        MCP_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            },
            "resources": {
                "listChanged": false,
                "subscribe": false
            },
            "prompts": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": env!("CARGO_PKG_NAME"),
            "title": "Codex Hermes Bridge",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Use these tools to inspect and control local Codex threads. Do not expose this server to untrusted agents or remote users."
    })
}

fn mcp_tool_call_result(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call requires params.name")?;
    let empty_args = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty_args);
    let payload = execute_mcp_tool(name, arguments)?;
    mcp_tool_success_result(payload)
}

fn mcp_tool_success_result(payload: Value) -> Result<Value> {
    Ok(json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&payload)?
        }],
        "structuredContent": payload,
        "isError": false
    }))
}

fn mcp_tool_error_result(error: &anyhow::Error) -> Value {
    let message = format!("{error:#}");
    json!({
        "content": [{
            "type": "text",
            "text": message
        }],
        "structuredContent": {
            "ok": false,
            "error": {
                "code": "tool_error",
                "message": message,
                "classified": classify_app_server_error_message(&format!("{error:#}"))
            }
        },
        "isError": true
    })
}

fn execute_mcp_tool(name: &str, arguments: &Value) -> Result<Value> {
    match name {
        "codex_doctor" => {
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
            serde_json::to_value(DoctorEnvelope {
                ok: true,
                codex: DoctorCodex {
                    resolved_path: resolved.path.display().to_string(),
                    source: resolved.source.to_string(),
                    version_stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                },
            })
            .map_err(Into::into)
        }
        "codex_threads" => {
            let limit = mcp_arg_u64(arguments, &["limit"], 25)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            let result = sync_state_from_live(&mut client, &conn, now, limit, false)?;
            Ok(json!({ "threads": result["threads"].clone() }))
        }
        "codex_waiting" => {
            let project = mcp_arg_string(arguments, &["project"])?;
            let limit = mcp_arg_u64(arguments, &["limit"], 25)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
            serde_json::to_value(list_waiting_from_db(&conn, project.as_deref(), limit)?)
                .map_err(Into::into)
        }
        "codex_inbox" => {
            let project = mcp_arg_string(arguments, &["project"])?;
            let status = mcp_arg_string(arguments, &["status"])?;
            let attention = mcp_arg_string(arguments, &["attention"])?;
            let waiting_on = mcp_arg_string(arguments, &["waitingOn", "waiting_on"])?;
            let limit = mcp_arg_u64(arguments, &["limit"], 25)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, limit.max(25), false)?;
            serde_json::to_value(list_inbox_from_db(
                &conn,
                now,
                project.as_deref(),
                status.as_deref(),
                attention.as_deref(),
                waiting_on.as_deref(),
                limit,
            )?)
            .map_err(Into::into)
        }
        "codex_show" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            let result = client.request(
                "thread/read",
                json!({
                    "threadId": thread_id,
                    "includeTurns": true
                }),
            )?;
            build_show_thread_result(Some(&conn), &thread_id, result)
        }
        "codex_new" => {
            let cwd = mcp_arg_string(arguments, &["cwd"])?;
            let message = mcp_arg_string(arguments, &["message", "prompt"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let follow = mcp_arg_bool(arguments, &["follow"], false)?;
            let stream = mcp_arg_bool(arguments, &["stream"], false)?;
            let duration = mcp_arg_u64(arguments, &["durationMs", "duration"], 3000)?;
            let poll_interval = mcp_arg_u64(arguments, &["pollIntervalMs", "poll_interval"], 1000)?;
            let events = mcp_arg_events(arguments, &["events"])?;
            if dry_run {
                Ok(start_new_thread_dry_run(cwd.as_deref(), message.as_deref()))
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let created =
                    client.request("thread/start", thread_start_params(cwd.as_deref()))?;
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
                attach_mcp_follow_if_requested(
                    result,
                    &mut client,
                    follow,
                    stream,
                    duration,
                    poll_interval,
                    events.as_deref(),
                )
            }
        }
        "codex_fork" => {
            let from_thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let message = mcp_arg_string(arguments, &["message", "prompt"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let follow = mcp_arg_bool(arguments, &["follow"], false)?;
            let stream = mcp_arg_bool(arguments, &["stream"], false)?;
            let duration = mcp_arg_u64(arguments, &["durationMs", "duration"], 3000)?;
            let poll_interval = mcp_arg_u64(arguments, &["pollIntervalMs", "poll_interval"], 1000)?;
            let events = mcp_arg_events(arguments, &["events"])?;
            if dry_run {
                Ok(fork_thread_dry_run(&from_thread_id, message.as_deref()))
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let forked =
                    client.request("thread/fork", json!({ "threadId": from_thread_id }))?;
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
                    fork_thread_live_result(&from_thread_id, message.as_deref(), forked, started);
                attach_mcp_follow_if_requested(
                    result,
                    &mut client,
                    follow,
                    stream,
                    duration,
                    poll_interval,
                    events.as_deref(),
                )
            }
        }
        "codex_reply" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let message = mcp_required_string(arguments, &["message", "prompt"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let follow = mcp_arg_bool(arguments, &["follow"], false)?;
            let stream = mcp_arg_bool(arguments, &["stream"], false)?;
            let duration = mcp_arg_u64(arguments, &["durationMs", "duration"], 3000)?;
            let poll_interval = mcp_arg_u64(arguments, &["pollIntervalMs", "poll_interval"], 1000)?;
            let events = mcp_arg_events(arguments, &["events"])?;
            let sent_at = now_millis()?;
            if dry_run {
                serde_json::to_value(ReplyResult {
                    ok: true,
                    action: "reply",
                    dry_run: true,
                    thread_id: &thread_id,
                    message: &message,
                    sent_at,
                })
                .map_err(Into::into)
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
                let started = client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [text_input_value(&message)]
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
                attach_mcp_follow_if_requested(
                    result,
                    &mut client,
                    follow,
                    stream,
                    duration,
                    poll_interval,
                    events.as_deref(),
                )
            }
        }
        "codex_approve" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let decision = mcp_required_string(arguments, &["decision"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let follow = mcp_arg_bool(arguments, &["follow"], false)?;
            let stream = mcp_arg_bool(arguments, &["stream"], false)?;
            let duration = mcp_arg_u64(arguments, &["durationMs", "duration"], 3000)?;
            let poll_interval = mcp_arg_u64(arguments, &["pollIntervalMs", "poll_interval"], 1000)?;
            let events = mcp_arg_events(arguments, &["events"])?;
            let normalized = decision.trim().to_lowercase();
            let sent_text = match normalized.as_str() {
                "approve" => "YES",
                "deny" => "NO",
                _ => bail!("Approval decision must be approve or deny"),
            };
            let sent_at = now_millis()?;
            if dry_run {
                serde_json::to_value(ApproveResult {
                    ok: true,
                    action: "approve",
                    dry_run: true,
                    thread_id: &thread_id,
                    decision: &normalized,
                    sent_text,
                    sent_at,
                })
                .map_err(Into::into)
            } else {
                let mut client = CodexAppServerClient::connect()?;
                let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
                let started = client.request(
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [text_input_value(sent_text)]
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
                attach_mcp_follow_if_requested(
                    result,
                    &mut client,
                    follow,
                    stream,
                    duration,
                    poll_interval,
                    events.as_deref(),
                )
            }
        }
        "codex_archive" => {
            let thread_ids = mcp_arg_string_list(arguments, &["threadIds", "thread_ids"])?;
            let project = mcp_arg_string(arguments, &["project"])?;
            let status = mcp_arg_string(arguments, &["status"])?;
            let attention = mcp_arg_string(arguments, &["attention"])?;
            let limit = mcp_arg_u64(arguments, &["limit"], 100)?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let yes = mcp_arg_bool(arguments, &["yes"], false)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            if !dry_run && thread_ids.is_empty() && !yes {
                bail!("Refusing bulk archive without yes=true or dryRun=true");
            }
            let mut client = if dry_run {
                None
            } else {
                Some(CodexAppServerClient::connect()?)
            };
            if !dry_run && thread_ids.is_empty() {
                if let Some(client) = client.as_mut() {
                    sync_state_from_live(client, &conn, now, 50, false)?;
                }
            }
            let selection = resolve_archive_targets(
                &conn,
                &thread_ids,
                project.as_deref(),
                status.as_deref(),
                attention.as_deref(),
                limit,
                now,
            )?;
            if !dry_run && selection.using_filter_selection && !yes {
                bail!("Refusing bulk archive without yes=true or dryRun=true");
            }
            if dry_run {
                let results = selection
                    .targets
                    .into_iter()
                    .map(|thread_id| json!({ "threadId": thread_id, "status": "would_archive" }))
                    .collect::<Vec<_>>();
                Ok(archive_result(true, results))
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
                Ok(archive_result(false, results))
            }
        }
        "codex_unarchive" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let dry_run = mcp_arg_bool(arguments, &["dryRun", "dry_run"], false)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let live_result = if dry_run {
                None
            } else {
                let mut client = CodexAppServerClient::connect()?;
                Some(client.request("thread/unarchive", json!({ "threadId": thread_id }))?)
            };
            unarchive_thread_result(&conn, &thread_id, dry_run, now, live_result)
        }
        "codex_away" => {
            let action = mcp_required_string(arguments, &["action"])?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            match action.as_str() {
                "on" => set_away_mode(&conn, true, now),
                "off" => set_away_mode(&conn, false, now),
                "status" => get_away_mode(&conn),
                _ => bail!("away action must be on, off, or status"),
            }
        }
        "codex_notify_away" => {
            let completed = mcp_arg_bool(arguments, &["completed"], true)?;
            let mark_delivered =
                mcp_arg_bool(arguments, &["markDelivered", "mark_delivered"], false)?;
            let now = now_millis()?;
            let db_path = state_db_path()?;
            let conn = create_state_db(&db_path)?;
            let mut client = CodexAppServerClient::connect()?;
            sync_state_from_live(&mut client, &conn, now, 50, true)?;
            build_notify_away_from_db(&conn, now, completed, mark_delivered)
        }
        _ => bail!("Unknown MCP tool: {name}"),
    }
}

fn attach_mcp_follow_if_requested(
    result: Value,
    client: &mut CodexAppServerClient,
    follow: bool,
    stream: bool,
    duration: u64,
    poll_interval: u64,
    events: Option<&str>,
) -> Result<Value> {
    if !follow {
        return Ok(result);
    }
    let db_path = state_db_path()?;
    let conn = create_state_db(&db_path)?;
    let filter = parse_event_filter(events);
    if let Some(thread_id) = result.get("threadId").and_then(Value::as_str) {
        attach_follow_result(
            result.clone(),
            client,
            &conn,
            FollowRun {
                thread_id,
                duration_ms: duration,
                poll_interval_ms: poll_interval,
                event_filter: filter.as_ref(),
                stream,
            },
        )
    } else {
        Ok(result)
    }
}

fn mcp_arg_value<'a>(arguments: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = arguments.as_object()?;
    keys.iter().find_map(|key| object.get(*key))
}

fn mcp_arg_string(arguments: &Value, keys: &[&str]) -> Result<Option<String>> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::String(value)) => Ok(normalized_message(Some(value))),
        Some(Value::Null) | None => Ok(None),
        Some(other) => bail!("argument {} must be a string, got {}", keys[0], other),
    }
}

fn mcp_required_string(arguments: &Value, keys: &[&str]) -> Result<String> {
    mcp_arg_string(arguments, keys)?.with_context(|| format!("argument {} is required", keys[0]))
}

fn mcp_arg_bool(arguments: &Value, keys: &[&str], default: bool) -> Result<bool> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::Null) | None => Ok(default),
        Some(other) => bail!("argument {} must be a boolean, got {}", keys[0], other),
    }
}

fn mcp_arg_u64(arguments: &Value, keys: &[&str], default: u64) -> Result<u64> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Number(value)) => value
            .as_u64()
            .with_context(|| format!("argument {} must be an unsigned integer", keys[0])),
        Some(Value::Null) | None => Ok(default),
        Some(other) => bail!(
            "argument {} must be an unsigned integer, got {}",
            keys[0],
            other
        ),
    }
}

fn mcp_arg_string_list(arguments: &Value, keys: &[&str]) -> Result<Vec<String>> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .with_context(|| format!("argument {} must contain only strings", keys[0]))
            })
            .collect(),
        Some(Value::String(value)) => Ok(value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect()),
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(other) => bail!(
            "argument {} must be a string array or comma-separated string, got {}",
            keys[0],
            other
        ),
    }
}

fn mcp_arg_events(arguments: &Value, keys: &[&str]) -> Result<Option<String>> {
    match mcp_arg_value(arguments, keys) {
        Some(Value::Array(values)) => {
            let mut events = Vec::new();
            for value in values {
                let event = value
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .with_context(|| format!("argument {} must contain only strings", keys[0]))?;
                events.push(event.to_string());
            }
            Ok((!events.is_empty()).then(|| events.join(",")))
        }
        Some(Value::String(value)) => Ok(normalized_message(Some(value))),
        Some(Value::Null) | None => Ok(None),
        Some(other) => bail!(
            "argument {} must be a string or string array, got {}",
            keys[0],
            other
        ),
    }
}

fn mcp_resources() -> Vec<Value> {
    vec![
        json!({
            "uri": "codex://threads",
            "name": "Codex Threads",
            "title": "Codex Threads",
            "description": "Recent Codex threads from the local bridge.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "codex://inbox",
            "name": "Codex Inbox",
            "title": "Codex Inbox",
            "description": "Actionable Codex inbox rows with suggested actions.",
            "mimeType": "application/json"
        }),
        json!({
            "uri": "codex://waiting",
            "name": "Codex Waiting",
            "title": "Codex Waiting Threads",
            "description": "Codex threads waiting on user input or approval.",
            "mimeType": "application/json"
        }),
    ]
}

fn mcp_read_resource(params: &Value) -> Result<Value> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .context("resources/read requires params.uri")?;
    let payload = match uri {
        "codex://threads" => execute_mcp_tool("codex_threads", &json!({ "limit": 25 }))?,
        "codex://inbox" => execute_mcp_tool("codex_inbox", &json!({ "limit": 25 }))?,
        "codex://waiting" => execute_mcp_tool("codex_waiting", &json!({ "limit": 25 }))?,
        _ if uri.starts_with("codex://thread/") => {
            let thread_id = uri.trim_start_matches("codex://thread/");
            if thread_id.trim().is_empty() {
                bail!("thread resource uri is missing a thread id");
            }
            execute_mcp_tool("codex_show", &json!({ "threadId": thread_id }))?
        }
        _ => bail!("unknown Codex resource uri: {uri}"),
    };

    Ok(json!({
        "contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": serde_json::to_string_pretty(&payload)?
        }]
    }))
}

fn mcp_prompts() -> Vec<Value> {
    vec![
        json!({
            "name": "codex_check_inbox",
            "title": "Check Codex Inbox",
            "description": "Ask Hermes to inspect current Codex work before taking action.",
            "arguments": []
        }),
        json!({
            "name": "codex_reply",
            "title": "Reply To Codex",
            "description": "Prepare an explicit reply action for a Codex thread.",
            "arguments": [
                {
                    "name": "threadId",
                    "description": "Codex thread id to reply to.",
                    "required": true
                },
                {
                    "name": "message",
                    "description": "Exact reply text to send.",
                    "required": true
                }
            ]
        }),
        json!({
            "name": "codex_approve",
            "title": "Approve Codex Prompt",
            "description": "Prepare an explicit approve or deny action for a Codex prompt.",
            "arguments": [
                {
                    "name": "threadId",
                    "description": "Codex thread id with the approval prompt.",
                    "required": true
                },
                {
                    "name": "decision",
                    "description": "Approval decision: approve or deny.",
                    "required": true
                }
            ]
        }),
    ]
}

fn mcp_get_prompt(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("prompts/get requires params.name")?;
    let empty_args = json!({});
    let arguments = params.get("arguments").unwrap_or(&empty_args);
    let text = match name {
        "codex_check_inbox" => {
            "Check Codex state before taking action. Start with codex_inbox or codex_waiting. Summarize what is waiting, which threads need the user, and which action you recommend next.".to_string()
        }
        "codex_reply" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let message = mcp_required_string(arguments, &["message", "prompt"])?;
            format!(
                "Use the codex_reply tool with threadId `{thread_id}` and message `{message}`. Do not change the message unless the user explicitly asks you to revise it."
            )
        }
        "codex_approve" => {
            let thread_id = mcp_required_string(arguments, &["threadId", "thread_id"])?;
            let decision = mcp_required_string(arguments, &["decision"])?;
            format!(
                "Use the codex_approve tool with threadId `{thread_id}` and decision `{decision}`. Only approve or deny when the user explicitly requested that decision."
            )
        }
        _ => bail!("unknown Codex prompt: {name}"),
    };

    Ok(json!({
        "description": match name {
            "codex_check_inbox" => "Inspect Codex inbox state",
            "codex_reply" => "Reply to a Codex thread",
            "codex_approve" => "Approve or deny a Codex prompt",
            _ => "Codex prompt"
        },
        "messages": [{
            "role": "user",
            "content": {
                "type": "text",
                "text": text
            }
        }]
    }))
}

fn mcp_tools() -> Vec<Value> {
    vec![
        mcp_tool(
            "codex_doctor",
            "Codex Doctor",
            "Inspect the local Codex executable that the bridge will control.",
            mcp_schema(json!({}), vec![]),
            mcp_annotations(true, false, true, false),
        ),
        mcp_tool(
            "codex_threads",
            "List Codex Threads",
            "Sync live Codex state and list recent threads.",
            mcp_schema(
                json!({
                    "limit": integer_property("Maximum number of recent threads to sync and return.")
                }),
                vec![],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_waiting",
            "List Waiting Threads",
            "List Codex threads that are waiting for user input or approval.",
            mcp_schema(
                json!({
                    "project": string_property("Optional project label to filter by."),
                    "limit": integer_property("Maximum number of waiting threads to return.")
                }),
                vec![],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_inbox",
            "List Codex Inbox",
            "List actionable Codex inbox rows with attention and suggested-action metadata.",
            mcp_schema(
                json!({
                    "project": string_property("Optional project label to filter by."),
                    "status": string_property("Optional status type to filter by."),
                    "attention": string_property("Optional attention reason to filter by."),
                    "waitingOn": string_property("Optional waiting owner filter, such as me, codex, or none."),
                    "limit": integer_property("Maximum number of inbox rows to return.")
                }),
                vec![],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_show",
            "Show Codex Thread",
            "Read one Codex thread with derived bridge state and recent local actions.",
            mcp_schema(
                json!({
                    "threadId": string_property("Codex thread id to read.")
                }),
                vec!["threadId"],
            ),
            mcp_annotations(true, false, false, false),
        ),
        mcp_tool(
            "codex_new",
            "Start Codex Thread",
            "Start a new Codex thread, optionally sending an initial message.",
            action_schema(json!({
                "cwd": string_property("Optional working directory for the new thread."),
                "message": string_property("Optional initial user message."),
                "dryRun": bool_property("Return the action shape without contacting Codex.")
            }), vec![]),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_fork",
            "Fork Codex Thread",
            "Fork an existing Codex thread, optionally sending a follow-up message to the fork.",
            action_schema(json!({
                "threadId": string_property("Codex thread id to fork."),
                "message": string_property("Optional message to send to the fork."),
                "dryRun": bool_property("Return the action shape without contacting Codex.")
            }), vec!["threadId"]),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_reply",
            "Reply To Codex Thread",
            "Resume a waiting Codex thread and send a user reply.",
            action_schema(json!({
                "threadId": string_property("Codex thread id to resume."),
                "message": string_property("Reply text to send."),
                "dryRun": bool_property("Return the action shape without contacting Codex.")
            }), vec!["threadId", "message"]),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_approve",
            "Approve Codex Prompt",
            "Approve or deny a waiting Codex approval prompt by sending YES or NO.",
            action_schema(json!({
                "threadId": string_property("Codex thread id to resume."),
                "decision": enum_property("Approval decision.", vec!["approve", "deny"]),
                "dryRun": bool_property("Return the action shape without contacting Codex.")
            }), vec!["threadId", "decision"]),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_archive",
            "Archive Codex Threads",
            "Archive explicit threads or a filtered inbox selection. Bulk filtered archive requires yes=true or dryRun=true.",
            mcp_schema(
                json!({
                    "threadIds": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Explicit thread ids to archive."
                    },
                    "project": string_property("Optional project filter for inbox selection."),
                    "status": string_property("Optional status filter for inbox selection."),
                    "attention": string_property("Optional attention filter for inbox selection."),
                    "limit": integer_property("Maximum number of filtered inbox rows to archive."),
                    "dryRun": bool_property("Return selected targets without archiving."),
                    "yes": bool_property("Required for non-dry-run filtered bulk archive.")
                }),
                vec![],
            ),
            mcp_annotations(false, true, false, true),
        ),
        mcp_tool(
            "codex_unarchive",
            "Unarchive Codex Thread",
            "Unarchive one Codex thread.",
            mcp_schema(
                json!({
                    "threadId": string_property("Codex thread id to unarchive."),
                    "dryRun": bool_property("Return the action shape without contacting Codex.")
                }),
                vec!["threadId"],
            ),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_away",
            "Manage Codex Away Mode",
            "Turn away mode on or off, or read its current state.",
            mcp_schema(
                json!({
                    "action": enum_property("Away-mode action.", vec!["on", "off", "status"])
                }),
                vec!["action"],
            ),
            mcp_annotations(false, false, true, false),
        ),
        mcp_tool(
            "codex_notify_away",
            "Build Away Notifications",
            "Build away-mode notification summaries from synced Codex thread state.",
            mcp_schema(
                json!({
                    "completed": bool_property("Include completed thread updates. Defaults to true."),
                    "markDelivered": bool_property("Mark emitted notifications as delivered.")
                }),
                vec![],
            ),
            mcp_annotations(false, false, false, true),
        ),
    ]
}

fn mcp_tool(
    name: &str,
    title: &str,
    description: &str,
    input_schema: Value,
    annotations: Value,
) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
        "annotations": annotations
    })
}

fn mcp_schema(properties: Value, required: Vec<&str>) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn action_schema(mut properties: Value, required: Vec<&str>) -> Value {
    if let Some(object) = properties.as_object_mut() {
        object.insert(
            "follow".to_string(),
            bool_property("Collect a short follow result after the action."),
        );
        object.insert(
            "stream".to_string(),
            bool_property("Include follow events in the returned action result."),
        );
        object.insert(
            "durationMs".to_string(),
            integer_property("Follow duration in milliseconds."),
        );
        object.insert(
            "pollIntervalMs".to_string(),
            integer_property("Follow poll interval in milliseconds."),
        );
        object.insert(
            "events".to_string(),
            json!({
                "oneOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ],
                "description": "Optional follow event filter."
            }),
        );
    }
    mcp_schema(properties, required)
}

fn mcp_annotations(
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
) -> Value {
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": open_world
    })
}

fn string_property(description: &str) -> Value {
    json!({
        "type": "string",
        "description": description
    })
}

fn bool_property(description: &str) -> Value {
    json!({
        "type": "boolean",
        "description": description
    })
}

fn integer_property(description: &str) -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "description": description
    })
}

fn enum_property(description: &str, values: Vec<&str>) -> Value {
    json!({
        "type": "string",
        "enum": values,
        "description": description
    })
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

        let _ = client.request("initialize", initialize_params())?;
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

fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "capabilities": {},
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
    use clap::CommandFactory;

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
        ]);
        assert!(
            fork_cli.is_ok(),
            "fork should accept TS follow flags: {fork_cli:?}"
        );
    }

    #[test]
    fn cli_exposes_version_and_command_descriptions() {
        let mut command = Cli::command();
        assert_eq!(command.get_version(), Some(env!("CARGO_PKG_VERSION")));

        let help = command.render_long_help().to_string();
        assert!(help.contains("Inspect Codex setup"));
        assert!(help.contains("Stream thread updates"));
    }

    #[test]
    fn cli_accepts_mcp_server_command() {
        let parsed = Cli::try_parse_from(["codex-hermes-bridge", "mcp"]);
        assert!(parsed.is_ok(), "mcp command should parse: {parsed:?}");

        let mut command = Cli::command();
        let help = command.render_long_help().to_string();
        assert!(help.contains("Run a stdio MCP server for Hermes"));
    }

    #[test]
    fn cli_accepts_hermes_install_command() {
        let parsed = Cli::try_parse_from([
            "codex-hermes-bridge",
            "hermes",
            "install",
            "--server-name",
            "codex",
            "--hermes-command",
            "hermes-se",
            "--bridge-command",
            "codex-hermes-bridge",
            "--dry-run",
        ]);
        assert!(parsed.is_ok(), "hermes install should parse: {parsed:?}");
    }

    #[test]
    fn cli_accepts_hermes_notification_install_flags() {
        let parsed = Cli::try_parse_from([
            "codex-hermes-bridge",
            "hermes",
            "install",
            "--hermes-command",
            "hermes-se",
            "--webhook-deliver",
            "telegram",
            "--webhook-deliver-chat-id",
            "12345",
            "--webhook-secret",
            "test-secret",
            "--dry-run",
        ]);
        assert!(
            parsed.is_ok(),
            "hermes install should accept notification lane flags: {parsed:?}"
        );
    }

    #[test]
    fn cli_accepts_hermes_post_webhook_command() {
        let parsed = Cli::try_parse_from([
            "codex-hermes-bridge",
            "hermes",
            "post-webhook",
            "--url",
            "http://localhost:8644/webhooks/codex-watch",
            "--secret",
            "test-secret",
        ]);
        assert!(
            parsed.is_ok(),
            "hermes post-webhook should parse: {parsed:?}"
        );
    }

    #[test]
    fn hermes_install_dry_run_builds_mcp_add_command() {
        let result = run_hermes_install(HermesInstallOptions {
            server_name: "codex",
            hermes_command: "hermes-se",
            bridge_command: "codex-hermes-bridge",
            webhook_name: "codex-watch",
            webhook_events: DEFAULT_HERMES_WEBHOOK_EVENTS,
            webhook_deliver: None,
            webhook_deliver_chat_id: None,
            webhook_secret: None,
            dry_run: true,
        })
        .expect("dry-run install result");

        assert_eq!(result["action"], "hermes_install");
        assert_eq!(result["dryRun"], true);
        assert_eq!(result["hermesCommand"], "hermes-se");
        assert_eq!(
            result["args"],
            json!([
                "mcp",
                "add",
                "codex",
                "--command",
                "codex-hermes-bridge",
                "--args",
                "mcp"
            ])
        );
    }

    #[test]
    fn hermes_install_dry_run_builds_notification_lane() {
        let result = run_hermes_install(HermesInstallOptions {
            server_name: "codex",
            hermes_command: "hermes-se",
            bridge_command: "codex-hermes-bridge",
            webhook_name: "codex-watch",
            webhook_events: DEFAULT_HERMES_WEBHOOK_EVENTS,
            webhook_deliver: Some("telegram"),
            webhook_deliver_chat_id: Some("12345"),
            webhook_secret: Some("test-secret"),
            dry_run: true,
        })
        .expect("dry-run install result");

        assert_eq!(result["notificationLane"]["configured"], true);
        assert_eq!(result["notificationLane"]["deliver"], "telegram");
        assert_eq!(
            result["notificationLane"]["webhookUrl"],
            "http://localhost:8644/webhooks/codex-watch"
        );
        assert_eq!(
            result["notificationLane"]["webhookArgs"],
            json!([
                "webhook",
                "subscribe",
                "codex-watch",
                "--description",
                "Proactive Codex thread notifications from codex-hermes-bridge",
                "--events",
                DEFAULT_HERMES_WEBHOOK_EVENTS,
                "--prompt",
                HERMES_WEBHOOK_PROMPT,
                "--deliver",
                "telegram",
                "--deliver-chat-id",
                "12345",
                "--secret",
                "test-secret"
            ])
        );
        let watcher = result["notificationLane"]["watcherCommand"]
            .as_str()
            .expect("watcher command");
        assert!(watcher.contains("watch --events"));
        assert!(watcher.contains("hermes post-webhook"));
        assert!(watcher.contains("--secret test-secret"));
    }

    #[test]
    fn parse_hermes_webhook_subscribe_output_extracts_url_and_secret() {
        let parsed = parse_hermes_webhook_subscribe_output(
            "Subscribed.\nURL: http://localhost:8644/webhooks/codex-watch\nSecret: abc123\n",
        );

        assert_eq!(
            parsed,
            HermesWebhookSubscription {
                url: Some("http://localhost:8644/webhooks/codex-watch".to_string()),
                secret: Some("abc123".to_string())
            }
        );
    }

    #[test]
    fn parse_http_url_handles_localhost_webhook() {
        let parsed =
            parse_http_url("http://localhost:8644/webhooks/codex-watch").expect("parsed URL");

        assert_eq!(
            parsed,
            HttpTarget {
                host: "localhost".to_string(),
                port: 8644,
                path: "/webhooks/codex-watch".to_string(),
                host_header: "localhost:8644".to_string()
            }
        );
    }

    #[test]
    fn hmac_sha256_hex_matches_known_vector() {
        assert_eq!(
            hmac_sha256_hex(b"key", b"The quick brown fox jumps over the lazy dog"),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn hermes_webhook_payload_includes_reply_route_marker() {
        let payload = hermes_webhook_payload(&json!({
            "type": "thread_waiting",
            "threadId": "thr_1",
            "thread": {
                "displayName": "Needs reply"
            }
        }));

        assert_eq!(payload["event_type"], "thread_waiting");
        assert_eq!(payload["thread_id"], "thr_1");
        assert_eq!(payload["reply_route"]["route_kind"], "codex_reply");
        assert_eq!(payload["reply_route"]["thread_id"], "thr_1");
        assert_eq!(
            payload["reply_route_marker"],
            "THREAD_ROUTE {\"route_kind\":\"codex_reply\",\"thread_id\":\"thr_1\"}"
        );
    }

    #[test]
    fn post_hermes_webhook_posts_signed_request_to_local_server() {
        fn header_end(data: &[u8]) -> Option<usize> {
            data.windows(4).position(|window| window == b"\r\n\r\n")
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("listener addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept webhook request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("read timeout");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).expect("read request");
                assert_ne!(read, 0, "client closed before request completed");
                request.extend_from_slice(&buffer[..read]);
                if let Some(end) = header_end(&request) {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .and_then(|raw| raw.parse::<usize>().ok())
                        })
                        .expect("content length");
                    if request.len() >= end + 4 + content_length {
                        break;
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\n\r\nOK")
                .expect("write response");
            String::from_utf8(request).expect("request utf8")
        });

        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_1",
            "updatedAt": 42
        });
        let result = post_hermes_webhook(
            &format!("http://{addr}/webhooks/codex-watch"),
            "test-secret",
            &event,
            Duration::from_secs(2),
        )
        .expect("post webhook");
        let request = server.join().expect("server thread");
        let (headers, body) = request.split_once("\r\n\r\n").expect("request body");
        let expected_signature = hmac_sha256_hex(b"test-secret", body.as_bytes());
        let payload: Value = serde_json::from_str(body).expect("payload json");

        assert_eq!(result["statusCode"], 202);
        assert!(headers.starts_with("POST /webhooks/codex-watch HTTP/1.1"));
        assert!(headers.contains("X-GitHub-Event: thread_waiting"));
        assert!(headers.contains("X-GitHub-Delivery: codex-thread_waiting-thr_1-42"));
        assert!(headers.contains(&format!("X-Hub-Signature-256: sha256={expected_signature}")));
        assert_eq!(payload["event_type"], "thread_waiting");
        assert_eq!(payload["thread_id"], "thr_1");
    }

    #[test]
    fn mcp_initialize_advertises_tools_capability() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "hermes-test", "version": "1.0.0" }
            }
        }))
        .expect("initialize response");

        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(
            response["result"]["capabilities"]["resources"]["listChanged"],
            false
        );
        assert_eq!(
            response["result"]["capabilities"]["prompts"]["listChanged"],
            false
        );
        assert_eq!(
            response["result"]["serverInfo"]["name"],
            "codex-hermes-bridge"
        );
    }

    #[test]
    fn mcp_tools_list_exposes_codex_control_surface_without_watch() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }))
        .expect("tools/list response");
        let tools = response["result"]["tools"].as_array().expect("tools array");
        let names = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<BTreeSet<_>>();

        for expected in [
            "codex_doctor",
            "codex_threads",
            "codex_inbox",
            "codex_waiting",
            "codex_show",
            "codex_new",
            "codex_reply",
            "codex_approve",
            "codex_fork",
            "codex_archive",
            "codex_unarchive",
            "codex_away",
            "codex_notify_away",
        ] {
            assert!(names.contains(expected), "missing MCP tool {expected}");
        }
        assert!(!names.contains("codex_watch"));

        let doctor = tools
            .iter()
            .find(|tool| tool["name"] == "codex_doctor")
            .expect("doctor tool");
        assert_eq!(doctor["annotations"]["readOnlyHint"], true);

        let archive = tools
            .iter()
            .find(|tool| tool["name"] == "codex_archive")
            .expect("archive tool");
        assert_eq!(archive["annotations"]["destructiveHint"], true);

        let reply = tools
            .iter()
            .find(|tool| tool["name"] == "codex_reply")
            .expect("reply tool");
        assert_eq!(
            reply["inputSchema"]["required"],
            json!(["threadId", "message"])
        );
    }

    #[test]
    fn mcp_tool_call_returns_structured_dry_run_result() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "codex_reply",
                "arguments": {
                    "threadId": "thr_1",
                    "message": "ship it",
                    "dryRun": true
                }
            }
        }))
        .expect("tools/call response");

        let result = &response["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["action"], "reply");
        assert_eq!(result["structuredContent"]["dry_run"], true);
        assert_eq!(result["structuredContent"]["thread_id"], "thr_1");
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"action\": \"reply\""));
    }

    #[test]
    fn mcp_tool_errors_return_tool_result_errors() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "codex_reply",
                "arguments": {
                    "threadId": "thr_1",
                    "dryRun": true
                }
            }
        }))
        .expect("tools/call response");

        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["structuredContent"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("argument message is required"));
    }

    #[test]
    fn mcp_stdio_server_handles_handshake_discovery_and_dry_run_call() {
        let input = [
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": MCP_PROTOCOL_VERSION }
            }),
            json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "codex_approve",
                    "arguments": {
                        "threadId": "thr_2",
                        "decision": "approve",
                        "dryRun": true
                    }
                }
            }),
        ]
        .into_iter()
        .map(|value| serde_json::to_string(&value).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        let mut output = Vec::new();

        run_mcp_server(std::io::Cursor::new(input), &mut output).expect("stdio MCP run");

        let lines = String::from_utf8(output)
            .expect("utf8 output")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("json line"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0]["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert!(lines[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "codex_approve"));
        assert_eq!(lines[2]["result"]["structuredContent"]["action"], "approve");
        assert_eq!(lines[2]["result"]["structuredContent"]["sent_text"], "YES");
    }

    #[test]
    fn mcp_resources_list_exposes_thread_context_resources() {
        let response = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "resources/list",
            "params": {}
        }))
        .expect("resources/list response");

        let resources = response["result"]["resources"]
            .as_array()
            .expect("resources array");
        assert!(resources
            .iter()
            .any(|resource| resource["uri"] == "codex://inbox"));
        assert!(resources
            .iter()
            .any(|resource| resource["uri"] == "codex://waiting"));
    }

    #[test]
    fn mcp_prompts_list_and_get_codex_reply_prompt() {
        let list = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "prompts/list",
            "params": {}
        }))
        .expect("prompts/list response");
        assert!(list["result"]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|prompt| prompt["name"] == "codex_reply"));

        let get = mcp_handle_message(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "prompts/get",
            "params": {
                "name": "codex_reply",
                "arguments": {
                    "threadId": "thr_1",
                    "message": "Continue with tests."
                }
            }
        }))
        .expect("prompts/get response");
        let text = get["result"]["messages"][0]["content"]["text"]
            .as_str()
            .expect("prompt text");
        assert!(text.contains("codex_reply"));
        assert!(text.contains("thr_1"));
        assert!(text.contains("Continue with tests."));
    }

    #[test]
    fn cli_rejects_experimental_realtime_flag_for_stable_surface() {
        let parsed = Cli::try_parse_from([
            "codex-hermes-bridge",
            "follow",
            "thr_1",
            "--experimental-realtime",
        ]);
        assert!(
            parsed.is_err(),
            "experimental realtime should not be part of the stable v0.1 CLI"
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
    fn app_server_initialize_params_use_stable_capabilities() {
        let params = initialize_params();
        assert_eq!(params["capabilities"], json!({}));
        assert_eq!(params["clientInfo"]["name"], "codex-hermes-bridge");
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
    fn notify_away_cli_can_exclude_completed_threads() {
        let cli = Cli::parse_from(["codex-hermes-bridge", "notify-away", "--no-completed"]);

        match cli.command {
            Commands::NotifyAway { completed, .. } => assert!(!completed),
            _ => panic!("expected notify-away command"),
        }
    }

    #[test]
    fn hermes_hook_notifies_on_thread_completed_event() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/print-hook-event.py");
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
    fn print_hook_reports_missing_stdin_cleanly() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/print-hook-event.py");
        let output = Command::new("python3")
            .arg(script)
            .stderr(Stdio::piped())
            .output()
            .expect("run print hook without stdin");

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Expected one JSON event"));
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
    fn follow_result_emits_started_and_snapshot_events() {
        let thread = json!({"id": "thr_follow", "name": "Follow me"});
        let events = build_follow_events(FollowEventsFixture {
            thread_id: "thr_follow",
            duration_ms: 1000,
            poll_interval_ms: 500,
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

        let direct = follow_result_summary("thr_follow", 3000, &events, false);
        assert_eq!(direct["ok"], true);
        assert_eq!(direct["action"], "follow");
        assert_eq!(direct["threadId"], "thr_follow");
        assert!(direct.get("events").is_none());

        let composed = follow_result_summary("thr_follow", 3000, &events, true);
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
        assert_eq!(thread_start_params(None), json!({}));
        assert_eq!(
            thread_start_params(Some("/tmp/project")),
            json!({"cwd": "/tmp/project"})
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

    #[test]
    fn watch_exec_hook_fails_on_nonzero_exit() {
        let err = run_exec_hook(
            "python3 -c \"import sys; sys.exit(7)\"",
            &json!({"type": "thread_waiting", "threadId": "thr_1"}),
        )
        .expect_err("nonzero hook exit should fail");
        assert!(format!("{err:#}").contains("exited with status"));
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
