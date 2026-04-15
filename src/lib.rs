use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod cli;
mod codex;
mod config;
mod projects;
mod state;

use crate::cli::{
    AwayCommands, Cli, Commands, DaemonCommands, HermesCommands, ProjectCommands, TelegramCommands,
};
use crate::codex::{
    attach_follow_result, build_show_thread_result, classify_app_server_error_message,
    collect_follow_events, filter_watch_events, follow_result_summary, fork_thread_dry_run,
    fork_thread_live_result, get_away_mode, normalized_message, parse_event_filter,
    resolve_codex_binary, run_exec_hook, set_away_mode, start_codex_watch_receiver,
    start_new_thread_dry_run, start_thread_in_cwd, sync_state_from_live, text_input_value,
    thread_cwd_from_response, thread_id_from_response, turn_start_params, wait_for_started_turn,
    watch_events_from_sync_result, watch_thread_error_event, CodexAppServerClient, FollowRun,
    TELEGRAM_TURN_SETTLE_POLL_MS, TELEGRAM_TURN_SETTLE_TIMEOUT_MS,
};
#[cfg(test)]
use crate::codex::{derive_pending_prompt, normalize_thread_snapshot};
use crate::projects::{
    build_registered_project, derive_project_label, ensure_unique_project_id,
    resolve_new_thread_request, resolve_project_query, slugify_project_token,
};
use crate::state::{
    archive_result, create_state_db, deliver_due_outbound_events, derive_thread_display_name,
    enqueue_outbound_event, get_setting_number, get_telegram_current_project_id,
    insert_telegram_callback_route, insert_telegram_command_route, insert_telegram_message_route,
    list_inbox_from_db, list_waiting_from_db, lookup_telegram_command_route,
    lookup_telegram_message_route, mark_telegram_command_route_used, observed_workspaces_from_db,
    pending_outbound_count, record_action, record_telegram_inbound_processed,
    record_transport_delivery, resolve_archive_targets, set_setting,
    set_telegram_current_project_id, should_emit_for_away_window, state_db_path,
    telegram_inbound_processed, transport_delivery_exists, unarchive_thread_result,
    update_telegram_callback_message_id, ObservedWorkspace, OutboxDeliverySummary,
    TelegramCallbackAction, TelegramCallbackRoute, TelegramCommandRouteKind,
};
#[cfg(test)]
use crate::state::{classify_inbox_item, create_state_db_in_memory, BridgeThreadSnapshot};
use clap::Parser;
pub(crate) use config::{
    daemon_config_path, load_daemon_config, merged_daemon_config, read_daemon_config_raw,
    redacted_daemon_config, resolve_telegram_bot_token, write_daemon_config, DaemonConfig,
    RegisteredProject, SetupOptions, TelegramConfig, TelegramSetupOptions,
};
pub(crate) use state::state_dir_path;

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
    bridge: DoctorBridge,
}

#[derive(Serialize)]
struct DoctorCodex {
    resolved_path: String,
    source: String,
    version_stdout: String,
}

#[derive(Serialize)]
struct DoctorBridge {
    config_path: String,
    config_exists: bool,
    telegram_configured: bool,
    daemon_service_path: String,
    daemon_service_exists: bool,
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

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Setup {
            bot_token,
            chat_id,
            allowed_user_id,
            events,
            bridge_command,
            daemon_label,
            install_daemon,
            start_daemon,
            register_hermes,
            hermes_server_name,
            hermes_command,
            pair_timeout_ms,
            dry_run,
        } => {
            let result = setup_result(SetupOptions {
                bot_token: bot_token.as_deref(),
                chat_id: chat_id.as_deref(),
                allowed_user_id: allowed_user_id.as_deref(),
                events: &events,
                bridge_command: &bridge_command,
                daemon_label: &daemon_label,
                install_daemon,
                start_daemon,
                register_hermes,
                hermes_server_name: &hermes_server_name,
                hermes_command: &hermes_command,
                dry_run,
                pair_timeout_ms,
            })?;
            println!("{}", serde_json::to_string(&result)?);
        }
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
                bridge: doctor_bridge()?,
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
        Commands::Daemon { command } => match command {
            DaemonCommands::Run {
                once,
                poll_interval,
                timeout_ms,
            } => {
                run_daemon(once, poll_interval, Duration::from_millis(timeout_ms))?;
            }
            DaemonCommands::Install {
                dry_run,
                label,
                bridge_command,
            } => {
                let result = install_daemon_service(&label, &bridge_command, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            DaemonCommands::Uninstall { dry_run, label } => {
                let result = uninstall_daemon_service(&label, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            DaemonCommands::Start { dry_run, label } => {
                let result = start_daemon_service(&label, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            DaemonCommands::Stop { dry_run, label } => {
                let result = stop_daemon_service(&label, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            DaemonCommands::Status { label } => {
                let result = daemon_service_status(&label)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            DaemonCommands::Logs { label } => {
                let result = daemon_service_logs(&label)?;
                println!("{}", serde_json::to_string(&result)?);
            }
        },
        Commands::Telegram { command } => match command {
            TelegramCommands::Setup {
                bot_token,
                chat_id,
                allowed_user_id,
                events,
                bridge_command,
                pair_timeout_ms,
                dry_run,
            } => {
                let result = telegram_setup_result(TelegramSetupOptions {
                    bot_token: bot_token.as_deref(),
                    chat_id: chat_id.as_deref(),
                    allowed_user_id: allowed_user_id.as_deref(),
                    events: &events,
                    bridge_command: &bridge_command,
                    dry_run,
                    pair_timeout_ms,
                })?;
                println!("{}", serde_json::to_string(&result)?);
            }
            TelegramCommands::Status => {
                let result = telegram_status_result()?;
                println!("{}", serde_json::to_string(&result)?);
            }
            TelegramCommands::Test {
                message,
                timeout_ms,
                dry_run,
            } => {
                let result =
                    telegram_test_result(&message, Duration::from_millis(timeout_ms), dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            TelegramCommands::Disable { dry_run } => {
                let result = telegram_disable_result(dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
        },
        Commands::Projects { command } => match command {
            ProjectCommands::List { observed_limit } => {
                let result = projects_list_result(observed_limit)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            ProjectCommands::Add {
                cwd,
                id,
                label,
                aliases,
                dry_run,
            } => {
                let result =
                    project_add_result(&cwd, id.as_deref(), label.as_deref(), &aliases, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            ProjectCommands::Import { limit, dry_run } => {
                let result = project_import_result(limit, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
            ProjectCommands::Remove { id, dry_run } => {
                let result = project_remove_result(&id, dry_run)?;
                println!("{}", serde_json::to_string(&result)?);
            }
        },
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
                let result = start_thread_in_cwd(&mut client, cwd.as_deref(), message.as_deref())?;
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
                let raw = raw.as_str();
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
                dry_run,
            } => {
                let result = run_hermes_install(HermesInstallOptions {
                    server_name: &server_name,
                    hermes_command: &hermes_command,
                    bridge_command: &bridge_command,
                    dry_run,
                })?;
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

fn importable_projects_from_observed(
    observed: &[ObservedWorkspace],
    existing_projects: &[RegisteredProject],
) -> Vec<RegisteredProject> {
    let mut projects = Vec::new();
    let mut existing_ids = existing_projects
        .iter()
        .map(|project| project.id.clone())
        .collect::<BTreeSet<_>>();
    let existing_cwds = existing_projects
        .iter()
        .map(|project| project.cwd.clone())
        .collect::<BTreeSet<_>>();
    for workspace in observed {
        if existing_cwds.contains(&workspace.cwd)
            || projects
                .iter()
                .any(|project: &RegisteredProject| project.cwd == workspace.cwd)
        {
            continue;
        }
        let base_id =
            slugify_project_token(&workspace.label).unwrap_or_else(|| "project".to_string());
        let id = ensure_unique_project_id(&base_id, &existing_ids);
        existing_ids.insert(id.clone());
        projects.push(RegisteredProject {
            id,
            label: workspace.label.clone(),
            cwd: workspace.cwd.clone(),
            aliases: Vec::new(),
        });
    }
    projects
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedTelegramDelivery {
    payloads: Vec<Value>,
    thread_id: Option<String>,
    event_id: String,
    callback_routes: Vec<TelegramCallbackRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TelegramInboundCommand {
    Start,
    Help,
    AwayOn,
    AwayOff,
    Status,
    NewThread(Option<String>),
    Project(Option<String>),
    Projects,
    Inbox,
    Waiting,
    Recent,
    Settings,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutedTelegramCommandPromptReply {
    kind: TelegramCommandRouteKind,
    message: String,
    project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutedTelegramReply {
    thread_id: String,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RoutedTelegramCallback {
    callback_query_id: String,
    thread_id: String,
    action: TelegramCallbackAction,
}

fn daemon_run_command(bridge_command: &str) -> String {
    format!("{} daemon run", shell_quote(bridge_command))
}

fn doctor_bridge() -> Result<DoctorBridge> {
    let config_path = daemon_config_path()?;
    let config = read_daemon_config_raw()?;
    let service = daemon_service_spec(DEFAULT_DAEMON_LABEL, "codex-telegram-bridge")?;
    Ok(DoctorBridge {
        config_path: config_path.display().to_string(),
        config_exists: config_path.exists(),
        telegram_configured: config
            .as_ref()
            .and_then(|config| config.telegram.as_ref())
            .is_some(),
        daemon_service_path: service.service_path.display().to_string(),
        daemon_service_exists: service.service_path.exists(),
    })
}

fn setup_result(options: SetupOptions<'_>) -> Result<Value> {
    let resolved = resolve_codex_binary()?;
    let telegram = telegram_setup_result(TelegramSetupOptions {
        bot_token: options.bot_token,
        chat_id: options.chat_id,
        allowed_user_id: options.allowed_user_id,
        events: options.events,
        bridge_command: options.bridge_command,
        dry_run: options.dry_run,
        pair_timeout_ms: options.pair_timeout_ms,
    })?;
    let daemon_install = if options.install_daemon {
        Some(install_daemon_service(
            options.daemon_label,
            options.bridge_command,
            options.dry_run,
        )?)
    } else {
        None
    };
    let daemon_start = if options.start_daemon {
        Some(start_daemon_service(options.daemon_label, options.dry_run)?)
    } else {
        None
    };
    let hermes = if options.register_hermes {
        Some(run_hermes_install(HermesInstallOptions {
            server_name: options.hermes_server_name,
            hermes_command: options.hermes_command,
            bridge_command: options.bridge_command,
            dry_run: options.dry_run,
        })?)
    } else {
        None
    };

    Ok(json!({
        "ok": true,
        "action": "setup",
        "dryRun": options.dry_run,
        "codex": {
            "resolvedPath": resolved.path.display().to_string(),
            "source": resolved.source
        },
        "telegram": telegram,
        "daemon": {
            "install": daemon_install,
            "start": daemon_start
        },
        "hermes": hermes,
        "nextStep": if options.dry_run {
            "Run setup without --dry-run, then use away on when leaving your computer."
        } else {
            "Use away on when leaving your computer. Reply to Codex Telegram messages to keep working from Telegram."
        }
    }))
}

fn telegram_setup_result(options: TelegramSetupOptions<'_>) -> Result<Value> {
    let bot_token = resolve_telegram_bot_token(options.bot_token)?;
    let events = options.events.trim();
    let bridge_command = options.bridge_command.trim();
    if events.is_empty() {
        bail!("telegram setup events cannot be empty");
    }
    if bridge_command.is_empty() {
        bail!("telegram setup bridge command cannot be empty");
    }
    if !options.dry_run {
        telegram_delete_webhook(&bot_token, Duration::from_secs(10))
            .context("failed to clear existing Telegram webhook before enabling long polling")?;
    }

    let pair_hint = json!({
        "message": "Send /start to the Telegram bot to pair this chat automatically.",
        "timeoutMs": options.pair_timeout_ms
    });
    let paired = if let Some(chat_id) = options.chat_id.map(str::trim).filter(|v| !v.is_empty()) {
        TelegramConfig {
            bot_token: bot_token.clone(),
            chat_id: chat_id.to_string(),
            allowed_user_id: options
                .allowed_user_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        }
    } else if options.dry_run {
        TelegramConfig {
            bot_token: bot_token.clone(),
            chat_id: "<paired by /start>".to_string(),
            allowed_user_id: options
                .allowed_user_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        }
    } else {
        discover_telegram_pairing(&bot_token, options.pair_timeout_ms)?
    };

    let existing = read_daemon_config_raw()?;
    let config = merged_daemon_config(existing.as_ref(), bridge_command, events, paired.clone());
    let commands = telegram_bot_commands();
    let commands_registration = if options.dry_run {
        json!({ "registered": false, "dryRun": true, "commands": commands })
    } else {
        json!({
            "registered": true,
            "commands": commands,
            "response": telegram_set_my_commands(&paired, Duration::from_secs(10))
                .context("failed to register Telegram slash commands")?
        })
    };

    let config_path = if options.dry_run {
        daemon_config_path()?
    } else {
        write_daemon_config(&config)?
    };

    Ok(json!({
        "ok": true,
        "action": "telegram_setup",
        "dryRun": options.dry_run,
        "configPath": config_path.display().to_string(),
        "telegram": {
            "configured": true,
            "botToken": "<redacted>",
            "chatId": paired.chat_id,
            "allowedUserId": paired.allowed_user_id,
            "pairing": if options.chat_id.is_some() { Value::Null } else { pair_hint },
            "commands": commands_registration
        },
        "config": redacted_daemon_config(&config),
        "daemonCommand": daemon_run_command(bridge_command),
        "daemonInstallCommand": format!("{} daemon install --bridge-command {}", shell_quote(bridge_command), shell_quote(bridge_command)),
        "nextStep": "Install and start the daemon. Codex updates will go directly to Telegram, Telegram replies route back to the originating thread, and slash commands control away mode or start new threads."
    }))
}

fn telegram_status_result() -> Result<Value> {
    let config = read_daemon_config_raw()?;
    Ok(json!({
        "ok": true,
        "action": "telegram_status",
        "configPath": daemon_config_path()?.display().to_string(),
        "configured": config.as_ref().and_then(|config| config.telegram.as_ref()).is_some(),
        "config": config.as_ref().map(redacted_daemon_config)
    }))
}

fn telegram_test_result(message: &str, timeout: Duration, dry_run: bool) -> Result<Value> {
    let config = load_daemon_config()?;
    let telegram = config
        .telegram
        .as_ref()
        .context("Telegram is not configured. Run telegram setup first.")?;
    let text = normalized_message(Some(message))
        .unwrap_or_else(|| "Codex Telegram bridge test".to_string());
    let payload = json!({
        "chat_id": telegram.chat_id,
        "text": text,
        "disable_web_page_preview": true
    });
    if dry_run {
        return Ok(json!({
            "ok": true,
            "action": "telegram_test",
            "dryRun": true,
            "payload": payload
        }));
    }
    let sent = telegram_send_message(telegram, &payload, timeout)?;
    Ok(json!({
        "ok": true,
        "action": "telegram_test",
        "dryRun": false,
        "messageId": sent.pointer("/result/message_id").cloned().unwrap_or(Value::Null)
    }))
}

fn telegram_disable_result(dry_run: bool) -> Result<Value> {
    let path = daemon_config_path()?;
    let config = read_daemon_config_raw()?;
    let had_telegram = config
        .as_ref()
        .and_then(|config| config.telegram.as_ref())
        .is_some();
    let removes_config = config.is_some();
    if !dry_run && removes_config && path.exists() {
        fs::remove_file(&path)?;
    }
    Ok(json!({
        "ok": true,
        "action": "telegram_disable",
        "dryRun": dry_run,
        "hadTelegram": had_telegram,
        "configPath": path.display().to_string(),
        "removedConfig": removes_config
    }))
}

fn projects_list_result(observed_limit: u64) -> Result<Value> {
    let config = load_daemon_config()?;
    let db_path = state_db_path()?;
    let conn = create_state_db(&db_path)?;
    let observed = observed_workspaces_from_db(&conn, observed_limit)?;
    let importable = importable_projects_from_observed(&observed, &config.projects);
    Ok(json!({
        "ok": true,
        "action": "projects_list",
        "configured": config.projects,
        "observed": observed.into_iter().map(|workspace| json!({
            "label": workspace.label,
            "cwd": workspace.cwd,
            "lastSeenAt": workspace.last_seen_at
        })).collect::<Vec<_>>(),
        "importable": importable
    }))
}

fn project_add_result(
    cwd: &str,
    id: Option<&str>,
    label: Option<&str>,
    aliases: &[String],
    dry_run: bool,
) -> Result<Value> {
    let mut config = load_daemon_config()?;
    let project = build_registered_project(cwd, id, label, aliases, &config.projects)?;
    if config
        .projects
        .iter()
        .any(|existing| existing.cwd == project.cwd)
    {
        bail!("project cwd `{}` is already registered", project.cwd);
    }
    if !dry_run {
        config.projects.push(project.clone());
        write_daemon_config(&config)?;
    }
    Ok(json!({
        "ok": true,
        "action": "projects_add",
        "dryRun": dry_run,
        "project": project
    }))
}

fn project_import_result(limit: u64, dry_run: bool) -> Result<Value> {
    let mut config = load_daemon_config()?;
    let db_path = state_db_path()?;
    let conn = create_state_db(&db_path)?;
    let observed = observed_workspaces_from_db(&conn, limit)?;
    let importable = importable_projects_from_observed(&observed, &config.projects);
    if !dry_run && !importable.is_empty() {
        config.projects.extend(importable.clone());
        write_daemon_config(&config)?;
    }
    Ok(json!({
        "ok": true,
        "action": "projects_import",
        "dryRun": dry_run,
        "imported": importable,
        "count": importable.len()
    }))
}

fn project_remove_result(id: &str, dry_run: bool) -> Result<Value> {
    let mut config = load_daemon_config()?;
    let Some(project) = config
        .projects
        .iter()
        .find(|project| project.id == id)
        .cloned()
    else {
        bail!("project `{id}` was not found");
    };
    if !dry_run {
        config.projects.retain(|candidate| candidate.id != id);
        write_daemon_config(&config)?;
    }
    Ok(json!({
        "ok": true,
        "action": "projects_remove",
        "dryRun": dry_run,
        "project": project
    }))
}

fn discover_telegram_pairing(bot_token: &str, timeout_ms: u64) -> Result<TelegramConfig> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let mut offset = None;
    while std::time::Instant::now() < deadline {
        let updates = telegram_get_updates(bot_token, offset, 10, Duration::from_secs(15))?;
        for update in telegram_updates_array(&updates)? {
            if let Some(update_id) = update.get("update_id").and_then(Value::as_i64) {
                offset = Some(update_id.saturating_add(1));
            }
            if let Some(message) = update.get("message") {
                let text = message.get("text").and_then(Value::as_str).unwrap_or("");
                if text.trim() != "/start" {
                    continue;
                }
                let chat_id = telegram_chat_id(message)
                    .context("Telegram /start update did not include chat.id")?;
                let allowed_user_id = telegram_from_user_id(message);
                return Ok(TelegramConfig {
                    bot_token: bot_token.to_string(),
                    chat_id,
                    allowed_user_id,
                });
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("Timed out waiting for Telegram /start. Send /start to the bot and rerun telegram setup.")
}

fn telegram_api_post(
    bot_token: &str,
    method: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let url = format!(
        "https://api.telegram.org/bot{}/{}",
        bot_token.trim(),
        method.trim()
    );
    let response = agent.post(&url).send_json(body.clone()).map_err(|error| {
        anyhow!(
            "Telegram API {method} request failed: {}",
            redact_secret_text(&error.to_string(), bot_token)
        )
    })?;
    let value: Value = response
        .into_json()
        .with_context(|| format!("Telegram API {method} returned invalid JSON"))?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("Telegram API {method} returned error: {value}");
    }
    Ok(value)
}

fn telegram_delete_webhook(bot_token: &str, timeout: Duration) -> Result<Value> {
    telegram_api_post(
        bot_token,
        "deleteWebhook",
        &json!({ "drop_pending_updates": false }),
        timeout,
    )
}

fn telegram_get_updates(
    bot_token: &str,
    offset: Option<i64>,
    timeout_seconds: u64,
    timeout: Duration,
) -> Result<Value> {
    let mut body = serde_json::Map::new();
    if let Some(offset) = offset {
        body.insert("offset".to_string(), json!(offset));
    }
    body.insert("timeout".to_string(), json!(timeout_seconds));
    body.insert(
        "allowed_updates".to_string(),
        json!(["message", "callback_query"]),
    );
    telegram_api_post(bot_token, "getUpdates", &Value::Object(body), timeout)
}

fn telegram_send_message(
    telegram: &TelegramConfig,
    payload: &Value,
    timeout: Duration,
) -> Result<Value> {
    telegram_api_post(&telegram.bot_token, "sendMessage", payload, timeout)
}

fn telegram_send_text(telegram: &TelegramConfig, text: &str, timeout: Duration) -> Result<Value> {
    telegram_send_message(
        telegram,
        &json!({
            "chat_id": telegram.chat_id.as_str(),
            "text": text,
            "disable_web_page_preview": true
        }),
        timeout,
    )
}

fn telegram_send_text_message_id(
    telegram: &TelegramConfig,
    text: &str,
    timeout: Duration,
) -> Result<i64> {
    telegram_send_text(telegram, text, timeout)?
        .pointer("/result/message_id")
        .and_then(Value::as_i64)
        .context("Telegram sendMessage response missing result.message_id")
}

fn telegram_bot_commands() -> Vec<Value> {
    vec![
        json!({ "command": "start", "description": "Pair and show remote control help" }),
        json!({ "command": "help", "description": "Show Telegram remote control commands" }),
        json!({ "command": "away_on", "description": "Enable away mode notifications" }),
        json!({ "command": "away_off", "description": "Disable away mode notifications" }),
        json!({ "command": "status", "description": "Show away mode and pending delivery status" }),
        json!({ "command": "new_thread", "description": "Start a new Codex thread from Telegram" }),
        json!({ "command": "project", "description": "Show or switch the current project" }),
        json!({ "command": "projects", "description": "List configured projects and suggestions" }),
        json!({ "command": "inbox", "description": "Show actionable Codex inbox rows" }),
        json!({ "command": "waiting", "description": "Show threads waiting for you" }),
        json!({ "command": "recent", "description": "Show recent Codex threads" }),
        json!({ "command": "settings", "description": "Show current Telegram bridge settings" }),
    ]
}

fn telegram_set_my_commands(telegram: &TelegramConfig, timeout: Duration) -> Result<Value> {
    telegram_api_post(
        &telegram.bot_token,
        "setMyCommands",
        &json!({ "commands": telegram_bot_commands() }),
        timeout,
    )
}

fn telegram_answer_callback_query(
    telegram: &TelegramConfig,
    callback_query_id: &str,
    text: &str,
    timeout: Duration,
) -> Result<Value> {
    telegram_api_post(
        &telegram.bot_token,
        "answerCallbackQuery",
        &json!({
            "callback_query_id": callback_query_id,
            "text": text,
            "show_alert": false
        }),
        timeout,
    )
}

fn telegram_updates_array(updates: &Value) -> Result<&[Value]> {
    updates
        .get("result")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .context("Telegram getUpdates response did not contain result array")
}

fn telegram_chat_id(message: &Value) -> Option<String> {
    message.pointer("/chat/id").and_then(|value| {
        value
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| value.as_str().map(str::to_string))
    })
}

fn telegram_from_user_id(message: &Value) -> Option<String> {
    message.pointer("/from/id").and_then(|value| {
        value
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| value.as_str().map(str::to_string))
    })
}

fn telegram_message_id(message: &Value) -> Option<i64> {
    message.get("message_id").and_then(Value::as_i64)
}

fn telegram_bot_id(bot_token: &str) -> String {
    sha256_hex(bot_token.as_bytes())[..16].to_string()
}

fn telegram_authorized(
    telegram: &TelegramConfig,
    chat_id: Option<&str>,
    user_id: Option<&str>,
) -> bool {
    if chat_id != Some(telegram.chat_id.as_str()) {
        return false;
    }
    match telegram.allowed_user_id.as_deref() {
        Some(allowed) => user_id == Some(allowed),
        None => true,
    }
}

fn parse_telegram_command_text(text: &str) -> Option<TelegramInboundCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let raw_command = parts.next().unwrap_or_default();
    let rest = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let command = raw_command
        .split_once('@')
        .map(|(name, _)| name)
        .unwrap_or(raw_command)
        .to_ascii_lowercase();
    match command.as_str() {
        "/start" => Some(TelegramInboundCommand::Start),
        "/help" => Some(TelegramInboundCommand::Help),
        "/away_on" => Some(TelegramInboundCommand::AwayOn),
        "/away_off" => Some(TelegramInboundCommand::AwayOff),
        "/status" => Some(TelegramInboundCommand::Status),
        "/new_thread" => Some(TelegramInboundCommand::NewThread(rest.map(str::to_string))),
        "/project" => Some(TelegramInboundCommand::Project(rest.map(str::to_string))),
        "/projects" => Some(TelegramInboundCommand::Projects),
        "/inbox" => Some(TelegramInboundCommand::Inbox),
        "/waiting" => Some(TelegramInboundCommand::Waiting),
        "/recent" => Some(TelegramInboundCommand::Recent),
        "/settings" => Some(TelegramInboundCommand::Settings),
        _ => Some(TelegramInboundCommand::Unknown(raw_command.to_string())),
    }
}

fn extract_telegram_command(
    message: &Value,
    telegram: &TelegramConfig,
) -> Result<Option<TelegramInboundCommand>> {
    let chat_id = telegram_chat_id(message);
    let user_id = telegram_from_user_id(message);
    if !telegram_authorized(telegram, chat_id.as_deref(), user_id.as_deref()) {
        return Ok(None);
    }
    if message.get("reply_to_message").is_some() {
        return Ok(None);
    }
    Ok(message
        .get("text")
        .and_then(Value::as_str)
        .and_then(parse_telegram_command_text))
}

const TELEGRAM_MESSAGE_CHAR_LIMIT: usize = 4096;
const TELEGRAM_CONTINUE_THREAD_HINT: &str =
    "💬 To continue this thread, use Telegram's Reply action on this message.";
const TELEGRAM_ANSWER_THREAD_HINT: &str =
    "💬 To answer Codex, use Telegram's Reply action on this message.";
const TELEGRAM_APPROVAL_HINT: &str =
    "Use the buttons below, or use Telegram's Reply action on this message.";

fn telegram_event_title(event_type: &str, event: &Value) -> &'static str {
    if telegram_event_is_approval(event) {
        return "🔐 Codex needs approval";
    }
    match event_type {
        "thread_waiting" => "🟡 Codex needs you",
        "thread_completed" => "✅ Codex finished",
        "thread_status_changed" => "🔄 Codex changed",
        _ => "🧵 Codex update",
    }
}

fn telegram_event_reply_hint(event_type: &str, event: &Value) -> &'static str {
    if telegram_event_is_approval(event) {
        TELEGRAM_APPROVAL_HINT
    } else {
        match event_type {
            "thread_waiting" => TELEGRAM_ANSWER_THREAD_HINT,
            _ => TELEGRAM_CONTINUE_THREAD_HINT,
        }
    }
}

fn telegram_event_display_name(event: &Value) -> String {
    event
        .pointer("/thread/displayName")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/thread/name").and_then(Value::as_str))
        .or_else(|| event.get("threadId").and_then(Value::as_str))
        .unwrap_or("Codex thread")
        .to_string()
}

fn telegram_event_detail(event: &Value) -> Option<String> {
    event
        .pointer("/thread/pendingPrompt/question")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/thread/lastPreview").and_then(Value::as_str))
        .or_else(|| event.get("lastPreview").and_then(Value::as_str))
        .filter(|value| !value.trim().is_empty())
        .map(sanitize_telegram_detail)
}

fn telegram_event_is_approval(event: &Value) -> bool {
    event
        .pointer("/thread/pendingPrompt/promptKind")
        .and_then(Value::as_str)
        == Some("approval")
        || event
            .pointer("/thread/pendingPrompt/kind")
            .and_then(Value::as_str)
            == Some("approval")
}

fn telegram_callback_id(event_id: &str, action: TelegramCallbackAction) -> String {
    let digest = sha256_hex(format!("{event_id}:{}", action.as_str()).as_bytes());
    format!("cb_{}", &digest[..24])
}

fn split_telegram_text(text: &str, max_chars: usize) -> Vec<String> {
    assert!(
        max_chars > 0,
        "Telegram message chunk size must be non-zero"
    );

    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut chunk_chars = 0;

    for ch in text.chars() {
        if chunk_chars == max_chars {
            chunks.push(std::mem::take(&mut chunk));
            chunk_chars = 0;
        }
        chunk.push(ch);
        chunk_chars += 1;
    }

    if !chunk.is_empty() {
        chunks.push(chunk);
    }

    if chunks.is_empty() {
        chunks.push(String::new());
    }

    chunks
}

fn sanitize_telegram_detail(detail: &str) -> String {
    let mut sanitized = String::with_capacity(detail.len());
    let mut rest = detail;
    while let Some(start) = rest.find('[') {
        let (before, candidate_start) = rest.split_at(start);
        sanitized.push_str(before);
        let Some(end_offset) = candidate_start.find(']') else {
            sanitized.push_str(candidate_start);
            return sanitized;
        };
        let candidate = &candidate_start[1..end_offset];
        if let Some(replacement) = compact_telegram_file_reference(candidate) {
            sanitized.push_str(&replacement);
            rest = &candidate_start[end_offset + 1..];
        } else {
            sanitized.push('[');
            rest = &candidate_start[1..];
        }
    }
    sanitized.push_str(rest);
    sanitized
}

fn compact_telegram_file_reference(candidate: &str) -> Option<String> {
    let normalized = candidate
        .strip_prefix("F:")
        .or_else(|| candidate.strip_prefix("f:"))
        .unwrap_or(candidate);
    if !normalized.starts_with('/') {
        return None;
    }
    let (path, line_ref) = normalized.split_once('†').unwrap_or((normalized, ""));
    let file_name = Path::new(path).file_name()?.to_string_lossy();
    let line_ref = line_ref.trim();
    if line_ref.is_empty() {
        Some(file_name.into_owned())
    } else {
        Some(format!("{file_name} {line_ref}"))
    }
}

fn prepare_telegram_delivery(chat_id: &str, event: &Value) -> Result<PreparedTelegramDelivery> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("codex_event");
    let event_id = notification_event_id(event);
    let thread_id = event_thread_id(event);
    let mut lines = vec![
        telegram_event_title(event_type, event).to_string(),
        format!("🧵 {}", telegram_event_display_name(event)),
    ];
    if let Some(project) = event.pointer("/thread/project").and_then(Value::as_str) {
        lines.push(format!("📁 {project}"));
    }
    if let Some(detail) = telegram_event_detail(event) {
        lines.push(String::new());
        lines.push(detail);
    }
    if thread_id.is_some() {
        lines.push(String::new());
        lines.push(telegram_event_reply_hint(event_type, event).to_string());
    }

    let mut payloads = split_telegram_text(&lines.join("\n"), TELEGRAM_MESSAGE_CHAR_LIMIT)
        .into_iter()
        .map(|text| {
            json!({
                "chat_id": chat_id,
                "text": text,
                "disable_web_page_preview": true
            })
        })
        .collect::<Vec<_>>();

    let mut callback_routes = Vec::new();
    if telegram_event_is_approval(event) {
        if let Some(thread_id) = thread_id.as_ref() {
            let approve_id = telegram_callback_id(&event_id, TelegramCallbackAction::Approve);
            let deny_id = telegram_callback_id(&event_id, TelegramCallbackAction::Deny);
            payloads[0]["reply_markup"] = json!({
                "inline_keyboard": [[
                    { "text": "✅ Approve", "callback_data": format!("codex:{approve_id}") },
                    { "text": "🛑 Deny", "callback_data": format!("codex:{deny_id}") }
                ]]
            });
            callback_routes.push(TelegramCallbackRoute {
                callback_id: approve_id,
                chat_id: chat_id.to_string(),
                message_id: None,
                thread_id: thread_id.clone(),
                action: TelegramCallbackAction::Approve,
            });
            callback_routes.push(TelegramCallbackRoute {
                callback_id: deny_id,
                chat_id: chat_id.to_string(),
                message_id: None,
                thread_id: thread_id.clone(),
                action: TelegramCallbackAction::Deny,
            });
        }
    }

    Ok(PreparedTelegramDelivery {
        payloads,
        thread_id,
        event_id,
        callback_routes,
    })
}

fn extract_telegram_reply_route(
    conn: &Connection,
    message: &Value,
    telegram: &TelegramConfig,
) -> Result<Option<RoutedTelegramReply>> {
    let chat_id = telegram_chat_id(message);
    let user_id = telegram_from_user_id(message);
    if !telegram_authorized(telegram, chat_id.as_deref(), user_id.as_deref()) {
        return Ok(None);
    }
    let reply_message_id = message
        .get("reply_to_message")
        .and_then(telegram_message_id);
    let Some(reply_message_id) = reply_message_id else {
        return Ok(None);
    };
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(text) = text else {
        return Ok(None);
    };
    let Some(chat_id) = chat_id else {
        return Ok(None);
    };
    let thread_id = lookup_telegram_message_route(conn, &chat_id, reply_message_id)?;
    Ok(thread_id.map(|thread_id| RoutedTelegramReply {
        thread_id,
        message: text.to_string(),
    }))
}

fn extract_telegram_command_prompt_reply(
    conn: &Connection,
    message: &Value,
    telegram: &TelegramConfig,
) -> Result<Option<RoutedTelegramCommandPromptReply>> {
    let chat_id = telegram_chat_id(message);
    let user_id = telegram_from_user_id(message);
    if !telegram_authorized(telegram, chat_id.as_deref(), user_id.as_deref()) {
        return Ok(None);
    }
    let Some(chat_id) = chat_id else {
        return Ok(None);
    };
    let Some(reply_message_id) = message
        .get("reply_to_message")
        .and_then(telegram_message_id)
    else {
        return Ok(None);
    };
    let Some(message_text) = message
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    let Some((kind, payload)) = lookup_telegram_command_route(conn, &chat_id, reply_message_id)?
    else {
        return Ok(None);
    };
    Ok(Some(RoutedTelegramCommandPromptReply {
        kind,
        message: message_text.to_string(),
        project_id: payload
            .as_ref()
            .and_then(|value| value.get("projectId"))
            .and_then(Value::as_str)
            .map(str::to_string),
    }))
}

fn extract_telegram_callback_route(
    conn: &Connection,
    callback_query: &Value,
    telegram: &TelegramConfig,
) -> Result<Option<RoutedTelegramCallback>> {
    let message = callback_query.get("message");
    let chat_id = message.and_then(telegram_chat_id);
    let user_id = callback_query
        .get("from")
        .and_then(|from| from.get("id"))
        .and_then(|value| {
            value
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| value.as_str().map(str::to_string))
        });
    if !telegram_authorized(telegram, chat_id.as_deref(), user_id.as_deref()) {
        return Ok(None);
    }
    let callback_query_id = callback_query
        .get("id")
        .and_then(Value::as_str)
        .context("callback query missing id")?;
    let Some(callback_id) = callback_query
        .get("data")
        .and_then(Value::as_str)
        .and_then(|data| data.strip_prefix("codex:"))
    else {
        return Ok(None);
    };
    let route = conn
        .query_row(
            "SELECT thread_id, action FROM telegram_callback_routes WHERE callback_id = ?1",
            params![callback_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(route.and_then(|(thread_id, action)| {
        TelegramCallbackAction::from_str(&action).map(|action| RoutedTelegramCallback {
            callback_query_id: callback_query_id.to_string(),
            thread_id,
            action,
        })
    }))
}

fn deliver_telegram_event(
    conn: &Connection,
    telegram: &TelegramConfig,
    event: &Value,
    now: u64,
    timeout: Duration,
) -> Result<Value> {
    let mut prepared = prepare_telegram_delivery(&telegram.chat_id, event)?;
    for route in &prepared.callback_routes {
        insert_telegram_callback_route(conn, route, now)?;
    }
    let mut message_ids = Vec::with_capacity(prepared.payloads.len());
    for payload in &prepared.payloads {
        let response = telegram_send_message(telegram, payload, timeout)?;
        let message_id = response
            .pointer("/result/message_id")
            .and_then(Value::as_i64)
            .context("Telegram sendMessage response missing result.message_id")?;
        if let Some(thread_id) = prepared.thread_id.as_deref() {
            insert_telegram_message_route(
                conn,
                &telegram.chat_id,
                message_id,
                thread_id,
                &prepared.event_id,
                now,
            )?;
        }
        message_ids.push(message_id);
    }
    let first_message_id = *message_ids
        .first()
        .context("Telegram delivery did not send any messages")?;
    for route in &mut prepared.callback_routes {
        route.message_id = Some(first_message_id);
        update_telegram_callback_message_id(conn, &route.callback_id, first_message_id)?;
    }
    Ok(json!({
        "ok": true,
        "transport": "telegram",
        "messageId": first_message_id,
        "messageIds": message_ids,
        "chunks": prepared.payloads.len(),
        "threadId": prepared.thread_id,
        "callbacks": prepared.callback_routes.len()
    }))
}

fn send_codex_reply_to_thread(
    conn: &Connection,
    thread_id: &str,
    message: &str,
    now: u64,
) -> Result<Value> {
    let mut client = CodexAppServerClient::connect()?;
    let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
    let started = client.request(
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [text_input_value(message)]
        }),
    )?;
    let started_turn_id = started
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let follow = wait_for_started_turn(
        &mut client,
        conn,
        thread_id,
        started_turn_id.as_deref(),
        TELEGRAM_TURN_SETTLE_TIMEOUT_MS,
        TELEGRAM_TURN_SETTLE_POLL_MS,
    )?;
    record_action(
        conn,
        thread_id,
        "telegram_reply",
        json!({
            "message": message,
            "resumed": resumed,
            "started": started,
            "follow": follow.clone(),
            "sentAt": now
        }),
        now,
    )?;
    Ok(json!({
        "ok": true,
        "action": "telegram_reply",
        "threadId": thread_id,
        "message": message,
        "follow": follow,
        "sentAt": now
    }))
}

fn send_codex_approval_to_thread(
    conn: &Connection,
    thread_id: &str,
    action: TelegramCallbackAction,
    now: u64,
) -> Result<Value> {
    let sent_text = match action {
        TelegramCallbackAction::Approve => "YES",
        TelegramCallbackAction::Deny => "NO",
    };
    let mut client = CodexAppServerClient::connect()?;
    let resumed = client.request("thread/resume", json!({ "threadId": thread_id }))?;
    let started = client.request(
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [text_input_value(sent_text)]
        }),
    )?;
    let started_turn_id = started
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let follow = wait_for_started_turn(
        &mut client,
        conn,
        thread_id,
        started_turn_id.as_deref(),
        TELEGRAM_TURN_SETTLE_TIMEOUT_MS,
        TELEGRAM_TURN_SETTLE_POLL_MS,
    )?;
    record_action(
        conn,
        thread_id,
        "telegram_approval",
        json!({
            "decision": action.as_str(),
            "sentText": sent_text,
            "resumed": resumed,
            "started": started,
            "follow": follow.clone(),
            "sentAt": now
        }),
        now,
    )?;
    Ok(json!({
        "ok": true,
        "action": "telegram_approval",
        "threadId": thread_id,
        "decision": action.as_str(),
        "sentText": sent_text,
        "follow": follow,
        "sentAt": now
    }))
}

fn current_project_for_identity<'a>(
    config: &'a DaemonConfig,
    conn: &Connection,
    chat_id: &str,
    user_id: Option<&str>,
) -> Result<Option<&'a RegisteredProject>> {
    if let Some(project_id) = get_telegram_current_project_id(conn, chat_id, user_id)? {
        if let Some(project) = config
            .projects
            .iter()
            .find(|project| project.id == project_id)
        {
            return Ok(Some(project));
        }
    }
    if config.projects.len() == 1 {
        return Ok(config.projects.first());
    }
    Ok(None)
}

fn telegram_projects_text(
    config: &DaemonConfig,
    current_project: Option<&RegisteredProject>,
    observed: &[ObservedWorkspace],
) -> String {
    let mut lines = vec!["Projects".to_string(), String::new()];
    match current_project {
        Some(project) => lines.push(format!("Current: {} ({})", project.id, project.label)),
        None => lines.push("Current: none selected".to_string()),
    }
    if config.projects.is_empty() {
        lines.push(String::new());
        lines.push("No projects are configured yet.".to_string());
    } else {
        lines.push(String::new());
        lines.push("Configured:".to_string());
        for project in &config.projects {
            let current = current_project
                .map(|current| current.id == project.id)
                .unwrap_or(false);
            let marker = if current { "•" } else { "-" };
            lines.push(format!(
                "{marker} {} - {}",
                project.id,
                trim_for_telegram_line(&project.label, 80)
            ));
            lines.push(format!("  {}", project.cwd));
        }
    }
    if !observed.is_empty() {
        lines.push(String::new());
        lines.push("Observed from recent Codex history:".to_string());
        for workspace in observed.iter().take(5) {
            lines.push(format!(
                "- {} - {}",
                workspace.label,
                trim_for_telegram_line(&workspace.cwd, 90)
            ));
        }
        lines.push(
            "Run `codex-telegram-bridge projects import` locally to promote observed workspaces into the curated registry."
                .to_string(),
        );
    }
    lines.push(String::new());
    lines.push("Use /project <id> to switch the current project.".to_string());
    lines.join("\n")
}

fn telegram_project_text(project: Option<&RegisteredProject>) -> String {
    match project {
        Some(project) => format!(
            "Current project\n\n{} ({})\n{}\n\nNew Telegram threads will start here until you switch again.",
            project.id, project.label, project.cwd
        ),
        None => "No current project is selected.\n\nUse /projects to inspect the registry, then /project <id> to choose one."
            .to_string(),
    }
}

fn telegram_help_text() -> String {
    [
        "Codex remote is ready.",
        "",
        "Use Telegram's Reply action on a Codex notification to continue that exact thread.",
        "",
        "/away_on - turn on away notifications",
        "/away_off - turn off away notifications",
        "/status - show bridge status",
        "/new_thread <prompt> - start a new Codex thread",
        "/new_thread - ask for a prompt in a reply",
        "/project <id> - switch the current project",
        "/projects - list configured projects",
        "/inbox - show actionable threads",
        "/waiting - show threads waiting for you",
        "/recent - show recent threads",
        "/settings - show Telegram bridge settings",
    ]
    .join("\n")
}

fn telegram_format_item_line(index: usize, title: &str, detail: Option<&str>) -> String {
    match detail.map(str::trim).filter(|value| !value.is_empty()) {
        Some(detail) => format!(
            "{}. {} - {}",
            index + 1,
            title,
            trim_for_telegram_line(detail, 120)
        ),
        None => format!("{}. {}", index + 1, title),
    }
}

fn trim_for_telegram_line(value: &str, max_chars: usize) -> String {
    let mut trimmed = value.trim().replace('\n', " ");
    if trimmed.chars().count() <= max_chars {
        return trimmed;
    }
    trimmed = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    trimmed.push_str("...");
    trimmed
}

fn telegram_status_text(conn: &Connection) -> Result<String> {
    let away = get_away_mode(conn)?;
    let pending = pending_outbound_count(conn)?;
    let waiting = list_waiting_from_db(conn, None, 5)?;
    let away_label = if away["away"].as_bool() == Some(true) {
        "on"
    } else {
        "off"
    };
    Ok(format!(
        "Codex remote status\n\nAway mode: {away_label}\nPending Telegram notifications: {pending}\nThreads waiting for you: {}\n\nUse /away_on or /away_off to change away mode.",
        waiting.summary.count
    ))
}

fn telegram_settings_text(
    telegram: &TelegramConfig,
    conn: &Connection,
    configured_projects: usize,
) -> Result<String> {
    let away = get_away_mode(conn)?;
    let allowed_user = telegram
        .allowed_user_id
        .as_deref()
        .unwrap_or("any user in this chat");
    let away_label = if away["away"].as_bool() == Some(true) {
        "on"
    } else {
        "off"
    };
    Ok(format!(
        "Telegram bridge settings\n\nChat: connected\nAllowed user: {allowed_user}\nAway mode: {away_label}\nConfigured projects: {configured_projects}\n\nNotifications keep Codex's final answer verbatim. To continue a thread, use Telegram's Reply action on that specific notification."
    ))
}

fn telegram_waiting_text(conn: &Connection) -> Result<String> {
    let waiting = list_waiting_from_db(conn, None, 5)?;
    if waiting.threads.is_empty() {
        return Ok("No Codex threads are waiting for you.".to_string());
    }
    let mut lines = vec!["Threads waiting for you:".to_string(), String::new()];
    for (index, thread) in waiting.threads.iter().enumerate() {
        lines.push(telegram_format_item_line(
            index,
            &thread.display_name,
            thread
                .prompt
                .question
                .as_deref()
                .or(thread.last_preview.as_deref()),
        ));
    }
    lines.push(String::new());
    lines.push(
        "Use Telegram's Reply action on the matching Codex notification to answer that thread."
            .to_string(),
    );
    Ok(lines.join("\n"))
}

fn telegram_inbox_text(conn: &Connection, now: u64) -> Result<String> {
    let inbox = list_inbox_from_db(conn, now, None, None, None, None, 5)?;
    if inbox.items.is_empty() {
        return Ok("Your Codex inbox is empty.".to_string());
    }
    let mut lines = vec![
        format!(
            "Codex inbox: {} item{} need attention.",
            inbox.summary.needs_attention,
            if inbox.summary.needs_attention == 1 {
                ""
            } else {
                "s"
            }
        ),
        String::new(),
    ];
    for (index, item) in inbox.items.iter().enumerate() {
        let detail = format!("{} - {}", item.waiting_on, item.suggested_action);
        lines.push(telegram_format_item_line(
            index,
            &item.display_name,
            Some(&detail),
        ));
    }
    lines.push(String::new());
    lines.push("Use /waiting for reply-only items, or reply to a Codex notification to continue that thread.".to_string());
    Ok(lines.join("\n"))
}

fn telegram_recent_text(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare(
        "SELECT thread_id, name, cwd, updated_at, last_preview
         FROM threads_cache
         ORDER BY COALESCE(updated_at, 0) DESC
         LIMIT 5",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok("No recent Codex threads are cached yet.".to_string());
    }
    let mut lines = vec!["Recent Codex threads:".to_string(), String::new()];
    for (index, (thread_id, name, cwd, _, last_preview)) in rows.iter().enumerate() {
        let project = derive_project_label(cwd.as_deref());
        let display_name = derive_thread_display_name(
            name.as_deref(),
            project.as_deref(),
            last_preview.as_deref(),
            thread_id,
        );
        lines.push(telegram_format_item_line(
            index,
            &display_name,
            last_preview.as_deref(),
        ));
    }
    Ok(lines.join("\n"))
}

fn start_new_thread_from_telegram(
    conn: &Connection,
    project: &RegisteredProject,
    message: &str,
    now: u64,
) -> Result<Value> {
    let mut client = CodexAppServerClient::connect()?;
    let result = start_thread_in_cwd(&mut client, Some(&project.cwd), Some(message))?;
    let thread_id = result
        .get("threadId")
        .and_then(Value::as_str)
        .context("Codex app-server thread/start response missing thread.id")?;
    record_action(
        conn,
        &thread_id,
        "telegram_new_thread",
        json!({
            "projectId": project.id,
            "projectLabel": project.label,
            "cwd": project.cwd,
            "message": message,
            "result": result.clone(),
            "sentAt": now
        }),
        now,
    )?;
    Ok(result)
}

fn telegram_new_thread_confirmation_text(
    project: &RegisteredProject,
    result: &Value,
) -> Result<String> {
    let cwd = result.get("cwd").and_then(Value::as_str);
    Ok(match cwd {
        Some(cwd) if !cwd.trim().is_empty() => format!(
            "Started a new Codex thread in {}.\n{cwd}\n\nUse Telegram's Reply action on this message to continue it.",
            project.label
        ),
        _ => format!(
            "Started a new Codex thread in {} with no explicit working directory reported back.\n\nUse Telegram's Reply action on this message to continue it.",
            project.label
        ),
    })
}

fn send_new_thread_confirmation(
    conn: &Connection,
    telegram: &TelegramConfig,
    project: &RegisteredProject,
    result: &Value,
    timeout: Duration,
    now: u64,
) -> Result<Value> {
    let thread_id = result
        .get("threadId")
        .and_then(Value::as_str)
        .context("new thread result missing threadId")?;
    let text = telegram_new_thread_confirmation_text(project, result)?;
    let message_id = telegram_send_text_message_id(telegram, &text, timeout)?;
    insert_telegram_message_route(
        conn,
        &telegram.chat_id,
        message_id,
        thread_id,
        &format!("telegram_new_thread:{thread_id}"),
        now,
    )?;
    Ok(json!({
        "ok": true,
        "action": "telegram_new_thread_confirmation",
        "threadId": thread_id,
        "messageId": message_id
    }))
}

fn send_new_thread_prompt_for_project(
    conn: &Connection,
    telegram: &TelegramConfig,
    project: &RegisteredProject,
    timeout: Duration,
    now: u64,
) -> Result<Value> {
    let text = format!(
        "What should Codex work on in {}?\n{}\n\nUse Telegram's Reply action on this message with the prompt for the new thread.",
        project.label, project.cwd
    );
    let message_id = telegram_send_text_message_id(telegram, &text, timeout)?;
    insert_telegram_command_route(
        conn,
        &telegram.chat_id,
        message_id,
        TelegramCommandRouteKind::NewThread,
        Some(&json!({ "projectId": project.id })),
        now,
    )?;
    Ok(json!({
        "ok": true,
        "action": "telegram_new_thread_prompt",
        "projectId": project.id,
        "messageId": message_id
    }))
}

fn execute_telegram_command(
    conn: &Connection,
    telegram: &TelegramConfig,
    message: &Value,
    command: TelegramInboundCommand,
    now: u64,
    timeout: Duration,
) -> Result<Value> {
    let chat_id = telegram_chat_id(message).context("Telegram command missing chat.id")?;
    let user_id = telegram_from_user_id(message);
    match command {
        TelegramInboundCommand::Start | TelegramInboundCommand::Help => {
            let sent = telegram_send_text(telegram, &telegram_help_text(), timeout)?;
            Ok(json!({ "ok": true, "action": "telegram_help", "sent": sent }))
        }
        TelegramInboundCommand::AwayOn => {
            let state = set_away_mode(conn, true, now)?;
            let sent = telegram_send_text(
                telegram,
                "Away mode is on. Codex final answers will be forwarded here.",
                timeout,
            )?;
            Ok(json!({ "ok": true, "action": "telegram_away_on", "state": state, "sent": sent }))
        }
        TelegramInboundCommand::AwayOff => {
            let state = set_away_mode(conn, false, now)?;
            let cleared = state
                .get("clearedPendingNotifications")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let sent = telegram_send_text(
                telegram,
                &format!("Away mode is off. Cleared {cleared} pending notification(s)."),
                timeout,
            )?;
            Ok(json!({ "ok": true, "action": "telegram_away_off", "state": state, "sent": sent }))
        }
        TelegramInboundCommand::Status => {
            let sent = telegram_send_text(telegram, &telegram_status_text(conn)?, timeout)?;
            Ok(json!({ "ok": true, "action": "telegram_status", "sent": sent }))
        }
        TelegramInboundCommand::NewThread(Some(prompt)) => {
            let config = load_daemon_config()?;
            let current_project =
                current_project_for_identity(&config, conn, &chat_id, user_id.as_deref())?;
            match resolve_new_thread_request(&config.projects, current_project, Some(&prompt)) {
                Ok(request) => {
                    if let Some(prompt) = request.prompt.as_deref() {
                        let result =
                            start_new_thread_from_telegram(conn, request.project, prompt, now)?;
                        let confirmation = send_new_thread_confirmation(
                            conn,
                            telegram,
                            request.project,
                            &result,
                            timeout,
                            now,
                        )?;
                        Ok(json!({
                            "ok": true,
                            "action": "telegram_new_thread",
                            "projectId": request.project.id,
                            "result": result,
                            "confirmation": confirmation
                        }))
                    } else {
                        send_new_thread_prompt_for_project(
                            conn,
                            telegram,
                            request.project,
                            timeout,
                            now,
                        )
                    }
                }
                Err(error) => {
                    let observed = observed_workspaces_from_db(conn, 5).unwrap_or_default();
                    let sent = telegram_send_text(
                        telegram,
                        &format!(
                            "{}\n\n{}",
                            error,
                            telegram_projects_text(&config, current_project, &observed)
                        ),
                        timeout,
                    )?;
                    Ok(json!({
                        "ok": true,
                        "action": "telegram_new_thread_needs_project",
                        "sent": sent
                    }))
                }
            }
        }
        TelegramInboundCommand::NewThread(None) => {
            let config = load_daemon_config()?;
            let current_project =
                current_project_for_identity(&config, conn, &chat_id, user_id.as_deref())?;
            match resolve_new_thread_request(&config.projects, current_project, None) {
                Ok(request) => send_new_thread_prompt_for_project(
                    conn,
                    telegram,
                    request.project,
                    timeout,
                    now,
                ),
                Err(error) => {
                    let observed = observed_workspaces_from_db(conn, 5).unwrap_or_default();
                    let sent = telegram_send_text(
                        telegram,
                        &format!(
                            "{}\n\n{}",
                            error,
                            telegram_projects_text(&config, current_project, &observed)
                        ),
                        timeout,
                    )?;
                    Ok(json!({
                        "ok": true,
                        "action": "telegram_new_thread_needs_project",
                        "sent": sent
                    }))
                }
            }
        }
        TelegramInboundCommand::Project(Some(query)) => {
            let config = load_daemon_config()?;
            match resolve_project_query(&config.projects, &query) {
                Ok(project) => {
                    set_telegram_current_project_id(
                        conn,
                        &chat_id,
                        user_id.as_deref(),
                        &project.id,
                    )?;
                    let sent = telegram_send_text(
                        telegram,
                        &telegram_project_text(Some(project)),
                        timeout,
                    )?;
                    Ok(json!({
                        "ok": true,
                        "action": "telegram_project_set",
                        "projectId": project.id,
                        "sent": sent
                    }))
                }
                Err(error) => {
                    let current_project =
                        current_project_for_identity(&config, conn, &chat_id, user_id.as_deref())?;
                    let observed = observed_workspaces_from_db(conn, 5).unwrap_or_default();
                    let sent = telegram_send_text(
                        telegram,
                        &format!(
                            "{}\n\n{}",
                            error,
                            telegram_projects_text(&config, current_project, &observed)
                        ),
                        timeout,
                    )?;
                    Ok(json!({
                        "ok": true,
                        "action": "telegram_project_not_found",
                        "sent": sent
                    }))
                }
            }
        }
        TelegramInboundCommand::Project(None) => {
            let config = load_daemon_config()?;
            let current_project =
                current_project_for_identity(&config, conn, &chat_id, user_id.as_deref())?;
            let sent =
                telegram_send_text(telegram, &telegram_project_text(current_project), timeout)?;
            Ok(json!({ "ok": true, "action": "telegram_project", "sent": sent }))
        }
        TelegramInboundCommand::Projects => {
            let config = load_daemon_config()?;
            let current_project =
                current_project_for_identity(&config, conn, &chat_id, user_id.as_deref())?;
            let observed = observed_workspaces_from_db(conn, 5).unwrap_or_default();
            let sent = telegram_send_text(
                telegram,
                &telegram_projects_text(&config, current_project, &observed),
                timeout,
            )?;
            Ok(json!({ "ok": true, "action": "telegram_projects", "sent": sent }))
        }
        TelegramInboundCommand::Inbox => {
            let sent = telegram_send_text(telegram, &telegram_inbox_text(conn, now)?, timeout)?;
            Ok(json!({ "ok": true, "action": "telegram_inbox", "sent": sent }))
        }
        TelegramInboundCommand::Waiting => {
            let sent = telegram_send_text(telegram, &telegram_waiting_text(conn)?, timeout)?;
            Ok(json!({ "ok": true, "action": "telegram_waiting", "sent": sent }))
        }
        TelegramInboundCommand::Recent => {
            let sent = telegram_send_text(telegram, &telegram_recent_text(conn)?, timeout)?;
            Ok(json!({ "ok": true, "action": "telegram_recent", "sent": sent }))
        }
        TelegramInboundCommand::Settings => {
            let config = load_daemon_config()?;
            let sent = telegram_send_text(
                telegram,
                &telegram_settings_text(telegram, conn, config.projects.len())?,
                timeout,
            )?;
            Ok(json!({ "ok": true, "action": "telegram_settings", "sent": sent }))
        }
        TelegramInboundCommand::Unknown(command) => {
            let sent = telegram_send_text(
                telegram,
                &format!("I don't know {command} yet.\n\n{}", telegram_help_text()),
                timeout,
            )?;
            Ok(
                json!({ "ok": true, "action": "telegram_unknown_command", "command": command, "sent": sent }),
            )
        }
    }
}

fn execute_telegram_command_prompt_reply(
    conn: &Connection,
    telegram: &TelegramConfig,
    message: &Value,
    route: RoutedTelegramCommandPromptReply,
    now: u64,
    timeout: Duration,
) -> Result<Value> {
    let chat_id =
        telegram_chat_id(message).context("Telegram command prompt reply missing chat.id")?;
    let user_id = telegram_from_user_id(message);
    let reply_message_id = message
        .get("reply_to_message")
        .and_then(telegram_message_id)
        .context("Telegram command prompt reply missing reply_to_message.message_id")?;
    match route.kind {
        TelegramCommandRouteKind::NewThread => {
            let config = load_daemon_config()?;
            let current_project =
                current_project_for_identity(&config, conn, &chat_id, user_id.as_deref())?;
            let project = match route.project_id.as_deref() {
                Some(project_id) => match config
                    .projects
                    .iter()
                    .find(|project| project.id == project_id)
                {
                    Some(project) => Some(project),
                    None => {
                        let observed = observed_workspaces_from_db(conn, 5).unwrap_or_default();
                        let sent = telegram_send_text(
                            telegram,
                            &format!(
                                "That project is no longer available. Pick a project first, then start the thread again.\n\n{}",
                                telegram_projects_text(&config, current_project, &observed)
                            ),
                            timeout,
                        )?;
                        mark_telegram_command_route_used(conn, &chat_id, reply_message_id, now)?;
                        return Ok(json!({
                            "ok": true,
                            "action": "telegram_new_thread_prompt_missing_project",
                            "sent": sent
                        }));
                    }
                },
                None => current_project,
            };
            let Some(project) = project else {
                let observed = observed_workspaces_from_db(conn, 5).unwrap_or_default();
                let sent = telegram_send_text(
                    telegram,
                    &format!(
                        "No project is selected for that prompt. Use /project <id> first, then try /new_thread again.\n\n{}",
                        telegram_projects_text(&config, current_project, &observed)
                    ),
                    timeout,
                )?;
                mark_telegram_command_route_used(conn, &chat_id, reply_message_id, now)?;
                return Ok(json!({
                    "ok": true,
                    "action": "telegram_new_thread_prompt_needs_project",
                    "sent": sent
                }));
            };
            let result = start_new_thread_from_telegram(conn, project, &route.message, now)?;
            mark_telegram_command_route_used(conn, &chat_id, reply_message_id, now)?;
            let confirmation =
                send_new_thread_confirmation(conn, telegram, project, &result, timeout, now)?;
            Ok(json!({
                "ok": true,
                "action": "telegram_new_thread_prompt_reply",
                "projectId": project.id,
                "result": result,
                "confirmation": confirmation
            }))
        }
    }
}

fn process_telegram_updates(
    conn: &Connection,
    telegram: &TelegramConfig,
    now: u64,
    timeout: Duration,
) -> Result<Value> {
    let bot_id = telegram_bot_id(&telegram.bot_token);
    let key = format!("telegram_offset:{bot_id}");
    let offset = get_setting_number(conn, &key)?.map(|value| value as i64 + 1);
    let updates = telegram_get_updates(&telegram.bot_token, offset, 0, timeout)?;
    let updates = telegram_updates_array(&updates)?;
    let mut seen = 0usize;
    let mut replies = 0usize;
    let mut command_prompt_replies = 0usize;
    let mut commands = 0usize;
    let mut callbacks = 0usize;
    let mut duplicate = 0usize;
    let mut ignored = 0usize;
    let mut max_update_id = None;
    for update in updates {
        seen += 1;
        let update_id = update.get("update_id").and_then(Value::as_i64);
        if let Some(update_id) = update_id {
            max_update_id =
                Some(max_update_id.map_or(update_id, |current: i64| current.max(update_id)));
            if telegram_inbound_processed(conn, &bot_id, update_id)? {
                duplicate += 1;
                continue;
            }
        }
        if let Some(message) = update.get("message") {
            if let Some(route) = extract_telegram_reply_route(conn, message, telegram)? {
                let result =
                    send_codex_reply_to_thread(conn, &route.thread_id, &route.message, now)?;
                if let Some(update_id) = update_id {
                    record_telegram_inbound_processed(
                        conn,
                        &bot_id,
                        update_id,
                        "telegram_reply",
                        &result,
                        now,
                    )?;
                }
                replies += 1;
            } else if let Some(route) =
                extract_telegram_command_prompt_reply(conn, message, telegram)?
            {
                let result = execute_telegram_command_prompt_reply(
                    conn, telegram, message, route, now, timeout,
                )?;
                if let Some(update_id) = update_id {
                    record_telegram_inbound_processed(
                        conn,
                        &bot_id,
                        update_id,
                        "telegram_command_prompt_reply",
                        &result,
                        now,
                    )?;
                }
                command_prompt_replies += 1;
            } else if let Some(command) = extract_telegram_command(message, telegram)? {
                let result =
                    execute_telegram_command(conn, telegram, message, command, now, timeout)?;
                if let Some(update_id) = update_id {
                    record_telegram_inbound_processed(
                        conn,
                        &bot_id,
                        update_id,
                        "telegram_command",
                        &result,
                        now,
                    )?;
                }
                commands += 1;
            } else {
                if let Some(update_id) = update_id {
                    record_telegram_inbound_processed(
                        conn,
                        &bot_id,
                        update_id,
                        "message_ignored",
                        &json!({ "ignored": true }),
                        now,
                    )?;
                }
                ignored += 1;
            }
        } else if let Some(callback_query) = update.get("callback_query") {
            match extract_telegram_callback_route(conn, callback_query, telegram)? {
                Some(route) => {
                    let result =
                        send_codex_approval_to_thread(conn, &route.thread_id, route.action, now)?;
                    if let Some(update_id) = update_id {
                        record_telegram_inbound_processed(
                            conn,
                            &bot_id,
                            update_id,
                            "callback_query",
                            &result,
                            now,
                        )?;
                    }
                    let _ = telegram_answer_callback_query(
                        telegram,
                        &route.callback_query_id,
                        "Sent to Codex",
                        timeout,
                    );
                    callbacks += 1;
                }
                None => {
                    if let Some(update_id) = update_id {
                        record_telegram_inbound_processed(
                            conn,
                            &bot_id,
                            update_id,
                            "callback_query_ignored",
                            &json!({ "ignored": true }),
                            now,
                        )?;
                    }
                    ignored += 1;
                }
            }
        }
    }
    if let Some(update_id) = max_update_id {
        set_setting(conn, &key, update_id as u64)?;
    }
    Ok(json!({
        "ok": true,
        "transport": "telegram",
        "seen": seen,
        "replies": replies,
        "commandPromptReplies": command_prompt_replies,
        "commands": commands,
        "callbacks": callbacks,
        "duplicate": duplicate,
        "ignored": ignored
    }))
}

fn event_observed_at(event: &Value) -> Option<u64> {
    event
        .get("updatedAt")
        .and_then(Value::as_u64)
        .or_else(|| event.get("observedAt").and_then(Value::as_u64))
        .or_else(|| event.pointer("/thread/updatedAt").and_then(Value::as_u64))
}

fn should_enqueue_daemon_notification(conn: &Connection, event: &Value) -> Result<bool> {
    let away_status = get_away_mode(conn)?;
    if !away_notifications_enabled_from_status(&away_status) {
        return Ok(false);
    }
    let away_started_at = away_status.get("awayStartedAt").and_then(Value::as_u64);
    Ok(should_emit_for_away_window(
        away_started_at,
        event_observed_at(event),
    ))
}

fn enqueue_daemon_notification_events(
    conn: &Connection,
    events: &[Value],
    now: u64,
) -> Result<usize> {
    let mut enqueued = 0usize;
    for event in events {
        if should_enqueue_daemon_notification(conn, event)?
            && enqueue_outbound_event(conn, event, now)?
        {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

fn away_notifications_enabled_from_status(away_status: &Value) -> bool {
    away_status.get("away").and_then(Value::as_bool) == Some(true)
}

fn away_notifications_enabled(conn: &Connection) -> Result<bool> {
    Ok(away_notifications_enabled_from_status(&get_away_mode(
        conn,
    )?))
}

fn deliver_outbound_events(
    conn: &Connection,
    config: &DaemonConfig,
    now: u64,
    timeout: Duration,
) -> Result<OutboxDeliverySummary> {
    deliver_due_outbound_events(conn, now, 100, |event| {
        let event_id = notification_event_id(event);
        let telegram = config
            .telegram
            .as_ref()
            .context("Telegram is not configured. Run setup first.")?;
        let result = if transport_delivery_exists(conn, &event_id, "telegram")? {
            json!({ "ok": true, "transport": "telegram", "skipped": "already_delivered" })
        } else {
            let result = deliver_telegram_event(conn, telegram, event, now, timeout)?;
            record_transport_delivery(conn, &event_id, "telegram", &result, now)?;
            result
        };
        Ok(json!({ "telegram": result }))
    })
}

fn daemon_cycle(
    conn: &Connection,
    config: &DaemonConfig,
    now: u64,
    timeout: Duration,
) -> Result<Value> {
    let filter = parse_event_filter(Some(&config.events));
    let events = match CodexAppServerClient::connect().and_then(|mut client| {
        let sync_result = sync_state_from_live(&mut client, conn, now, 50, true)?;
        Ok(watch_events_from_sync_result(
            &sync_result,
            client.drain_notifications(),
            filter.as_ref(),
        ))
    }) {
        Ok(events) => events,
        Err(error) => filter_watch_events(vec![watch_thread_error_event(&error)], filter.as_ref()),
    };
    let enqueued = enqueue_daemon_notification_events(conn, &events, now)?;
    let delivery = if away_notifications_enabled(conn)? {
        deliver_outbound_events(conn, config, now, timeout)?
    } else {
        OutboxDeliverySummary::default()
    };
    let telegram_updates = match config.telegram.as_ref() {
        Some(telegram) => match process_telegram_updates(conn, telegram, now, timeout) {
            Ok(result) => result,
            Err(error) => json!({
                "ok": false,
                "transport": "telegram",
                "error": format!("{error:#}")
            }),
        },
        None => Value::Null,
    };
    Ok(json!({
        "ok": true,
        "action": "daemon_cycle",
        "observed": events.len(),
        "enqueued": enqueued,
        "delivery": delivery,
        "telegramUpdates": telegram_updates,
        "pending": pending_outbound_count(conn)?
    }))
}

fn run_daemon(once: bool, poll_interval: u64, timeout: Duration) -> Result<()> {
    let config = load_daemon_config()?;
    let db_path = state_db_path()?;
    let conn = create_state_db(&db_path)?;
    if once {
        let result = daemon_cycle(&conn, &config, now_millis()?, timeout)?;
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }

    let telegram_commands = config.telegram.as_ref().map(|telegram| {
        telegram_set_my_commands(telegram, timeout)
            .map(|_| json!({ "registered": true }))
            .unwrap_or_else(|error| {
                json!({
                    "registered": false,
                    "error": format!("{error:#}")
                })
            })
    });

    println!(
        "{}",
        serde_json::to_string(&json!({
            "ok": true,
            "action": "daemon_started",
            "configPath": daemon_config_path()?.display().to_string(),
            "events": config.events,
            "telegramCommands": telegram_commands
        }))?
    );
    let watch_rx = start_codex_watch_receiver().ok();
    loop {
        let result = daemon_cycle(&conn, &config, now_millis()?, timeout)?;
        println!("{}", serde_json::to_string(&result)?);
        if let Some(rx) = watch_rx.as_ref() {
            rx.recv_timeout(Duration::from_millis(poll_interval));
        } else {
            thread::sleep(Duration::from_millis(poll_interval));
        }
    }
}

#[derive(Debug, Clone)]
struct DaemonServiceSpec {
    service_path: PathBuf,
    stdout_log: PathBuf,
    stderr_log: PathBuf,
    unit_name: String,
    contents: String,
    install_command: String,
    uninstall_command: String,
    start_command: String,
    stop_command: String,
    status_command: String,
}

fn validate_daemon_label(label: &str) -> Result<&str> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        bail!("daemon label cannot be empty");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\'') {
        bail!("daemon label contains unsupported characters");
    }
    Ok(trimmed)
}

fn daemon_service_spec(label: &str, bridge_command: &str) -> Result<DaemonServiceSpec> {
    let label = validate_daemon_label(label)?;
    let bridge_command = bridge_command.trim();
    if bridge_command.is_empty() {
        bail!("bridge command cannot be empty");
    }
    let service_bridge_command = resolve_service_bridge_command(bridge_command);
    let state_dir = state_dir_path()?;
    let stdout_log = state_dir.join("daemon.out.log");
    let stderr_log = state_dir.join("daemon.err.log");
    let run_args = [
        service_bridge_command.clone(),
        "daemon".to_string(),
        "run".to_string(),
    ];
    if cfg!(target_os = "macos") {
        let service_path = dirs::home_dir()
            .context("home directory is not available")?
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist"));
        let args_xml = run_args
            .iter()
            .map(|arg| format!("        <string>{}</string>", xml_escape(arg)))
            .collect::<Vec<_>>()
            .join("\n");
        let contents = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
{}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
            xml_escape(label),
            args_xml,
            xml_escape(&stdout_log.display().to_string()),
            xml_escape(&stderr_log.display().to_string())
        );
        let quoted_path = shell_quote(&service_path.display().to_string());
        let bootstrap_command = format!("launchctl bootstrap gui/$(id -u) {quoted_path}");
        let kickstart_command =
            format!("launchctl kickstart -k gui/$(id -u)/{}", shell_quote(label));
        let bootout_command = format!("launchctl bootout gui/$(id -u)/{}", shell_quote(label));
        Ok(DaemonServiceSpec {
            service_path,
            stdout_log,
            stderr_log,
            unit_name: label.to_string(),
            contents,
            install_command: bootstrap_command.clone(),
            uninstall_command: format!("{bootout_command} 2>/dev/null || true"),
            start_command: format!("{bootstrap_command} 2>/dev/null || {kickstart_command}"),
            stop_command: bootout_command,
            status_command: format!("launchctl print gui/$(id -u)/{}", shell_quote(label)),
        })
    } else if cfg!(target_os = "linux") {
        let unit_name = if label.ends_with(".service") {
            label.to_string()
        } else {
            format!("{label}.service")
        };
        let service_path = dirs::home_dir()
            .context("home directory is not available")?
            .join(".config")
            .join("systemd")
            .join("user")
            .join(&unit_name);
        let contents = format!(
            "[Unit]\nDescription=Codex Telegram Bridge notification daemon\n\n[Service]\nType=simple\nExecStart={} daemon run\nRestart=always\nRestartSec=2\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
            shell_quote(&service_bridge_command),
            stdout_log.display(),
            stderr_log.display()
        );
        Ok(DaemonServiceSpec {
            service_path,
            stdout_log,
            stderr_log,
            unit_name: unit_name.clone(),
            contents,
            install_command: format!(
                "systemctl --user daemon-reload && systemctl --user enable --now {}",
                shell_quote(&unit_name)
            ),
            uninstall_command: format!(
                "systemctl --user disable --now {} 2>/dev/null || true",
                shell_quote(&unit_name)
            ),
            start_command: format!(
                "systemctl --user daemon-reload && systemctl --user enable --now {}",
                shell_quote(&unit_name)
            ),
            stop_command: format!("systemctl --user stop {}", shell_quote(&unit_name)),
            status_command: format!("systemctl --user status {}", shell_quote(&unit_name)),
        })
    } else {
        bail!("daemon service install is only supported on macOS launchd and Linux systemd")
    }
}

fn resolve_service_bridge_command(bridge_command: &str) -> String {
    let trimmed = bridge_command.trim();
    if trimmed.contains('/') {
        let path = PathBuf::from(trimmed);
        return if path.is_absolute() {
            path.display().to_string()
        } else {
            env::current_dir()
                .map(|cwd| cwd.join(path).display().to_string())
                .unwrap_or_else(|_| trimmed.to_string())
        };
    }
    which::which(trimmed)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| trimmed.to_string())
}

fn install_daemon_service(label: &str, bridge_command: &str, dry_run: bool) -> Result<Value> {
    let spec = daemon_service_spec(label, bridge_command)?;
    if !dry_run {
        if let Some(parent) = spec.service_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&spec.service_path, &spec.contents)?;
    }
    Ok(json!({
        "ok": true,
        "action": "daemon_install",
        "dryRun": dry_run,
        "label": spec.unit_name,
        "servicePath": spec.service_path,
        "runCommand": daemon_run_command(bridge_command),
        "installCommand": spec.install_command,
        "startCommand": spec.start_command,
        "stopCommand": spec.stop_command,
        "statusCommand": spec.status_command,
        "logs": {
            "stdout": spec.stdout_log,
            "stderr": spec.stderr_log
        },
        "contents": if dry_run { Some(spec.contents) } else { None }
    }))
}

fn uninstall_daemon_service(label: &str, dry_run: bool) -> Result<Value> {
    let spec = daemon_service_spec(label, "codex-telegram-bridge")?;
    let output = if dry_run {
        None
    } else {
        Some(run_shell_command(&spec.uninstall_command)?)
    };
    if !dry_run && spec.service_path.exists() {
        fs::remove_file(&spec.service_path)?;
    }
    Ok(json!({
        "ok": true,
        "action": "daemon_uninstall",
        "dryRun": dry_run,
        "label": spec.unit_name,
        "servicePath": spec.service_path,
        "uninstallCommand": spec.uninstall_command,
        "output": output
    }))
}

fn run_shell_command(command: &str) -> Result<Value> {
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .output()
        .with_context(|| format!("failed to run `{command}`"))?;
    Ok(json!({
        "status": output.status.code(),
        "success": output.status.success(),
        "stdout": String::from_utf8_lossy(&output.stdout).trim(),
        "stderr": String::from_utf8_lossy(&output.stderr).trim()
    }))
}

fn start_daemon_service(label: &str, dry_run: bool) -> Result<Value> {
    let spec = daemon_service_spec(label, "codex-telegram-bridge")?;
    let command = spec.start_command.clone();
    let output = if dry_run {
        None
    } else {
        Some(run_shell_command(&command)?)
    };
    Ok(json!({
        "ok": true,
        "action": "daemon_start",
        "dryRun": dry_run,
        "label": spec.unit_name,
        "command": command,
        "output": output
    }))
}

fn stop_daemon_service(label: &str, dry_run: bool) -> Result<Value> {
    let spec = daemon_service_spec(label, "codex-telegram-bridge")?;
    let command = spec.stop_command.clone();
    let output = if dry_run {
        None
    } else {
        Some(run_shell_command(&command)?)
    };
    Ok(json!({
        "ok": true,
        "action": "daemon_stop",
        "dryRun": dry_run,
        "label": spec.unit_name,
        "command": command,
        "output": output
    }))
}

fn daemon_service_status(label: &str) -> Result<Value> {
    let spec = daemon_service_spec(label, "codex-telegram-bridge")?;
    let config_path = daemon_config_path()?;
    Ok(json!({
        "ok": true,
        "action": "daemon_status",
        "label": spec.unit_name,
        "configPath": config_path,
        "configExists": config_path.exists(),
        "servicePath": spec.service_path,
        "serviceExists": spec.service_path.exists(),
        "statusCommand": spec.status_command,
        "logs": {
            "stdout": spec.stdout_log,
            "stderr": spec.stderr_log
        }
    }))
}

fn daemon_service_logs(label: &str) -> Result<Value> {
    let spec = daemon_service_spec(label, "codex-telegram-bridge")?;
    Ok(json!({
        "ok": true,
        "action": "daemon_logs",
        "label": spec.unit_name,
        "stdout": spec.stdout_log,
        "stderr": spec.stderr_log,
        "tailCommand": format!(
            "tail -f {} {}",
            shell_quote(&spec.stdout_log.display().to_string()),
            shell_quote(&spec.stderr_log.display().to_string())
        )
    }))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

const DEFAULT_DAEMON_LABEL: &str = "com.hanifcarroll.codex-telegram-bridge";
const DEFAULT_NOTIFICATION_EVENTS: &str = "thread_waiting,thread_completed";

#[derive(Debug, Clone, Copy)]
struct HermesInstallOptions<'a> {
    server_name: &'a str,
    hermes_command: &'a str,
    bridge_command: &'a str,
    dry_run: bool,
}

fn run_hermes_install(options: HermesInstallOptions<'_>) -> Result<Value> {
    let server_name = options.server_name.trim();
    let hermes_command = options.hermes_command.trim();
    let bridge_command = options.bridge_command.trim();

    if server_name.is_empty() {
        bail!("server name cannot be empty");
    }
    if hermes_command.is_empty() {
        bail!("hermes command cannot be empty");
    }
    if bridge_command.is_empty() {
        bail!("bridge command cannot be empty");
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
        "nextStep": "Restart Hermes for MCP discovery. Telegram notifications are configured with the top-level setup command, not through Hermes."
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
        "nextStep": "Restart Hermes for MCP discovery. Telegram notifications are configured with the top-level setup command, not through Hermes."
    }))
}

fn redact_secret_text(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "<redacted>")
    }
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

fn event_thread_id(event: &Value) -> Option<String> {
    event
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/thread/threadId").and_then(Value::as_str))
        .or_else(|| event.pointer("/thread/id").and_then(Value::as_str))
        .map(str::to_string)
}

fn notification_event_id(event: &Value) -> String {
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
            "title": "Codex Telegram Bridge",
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
                bridge: doctor_bridge()?,
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
            "codex_reply",
            "Reply To Codex Thread",
            "Resume a waiting Codex thread and send a user reply.",
            action_schema(
                json!({
                    "threadId": string_property("Codex thread id to resume."),
                    "message": string_property("Reply text to send."),
                    "dryRun": bool_property("Return the action shape without contacting Codex.")
                }),
                vec!["threadId", "message"],
            ),
            mcp_annotations(false, false, false, true),
        ),
        mcp_tool(
            "codex_approve",
            "Approve Codex Prompt",
            "Approve or deny a waiting Codex approval prompt by sending YES or NO.",
            action_schema(
                json!({
                    "threadId": string_property("Codex thread id to resume."),
                    "decision": enum_property("Approval decision.", vec!["approve", "deny"]),
                    "dryRun": bool_property("Return the action shape without contacting Codex.")
                }),
                vec!["threadId", "decision"],
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
    fn telegram_setup_dry_run_writes_redacted_daemon_shape() {
        let result = telegram_setup_result(TelegramSetupOptions {
            bot_token: Some("123:secret"),
            chat_id: Some("456"),
            allowed_user_id: Some("789"),
            events: DEFAULT_NOTIFICATION_EVENTS,
            bridge_command: "codex-telegram-bridge",
            dry_run: true,
            pair_timeout_ms: 1000,
        })
        .expect("telegram setup dry run");

        assert_eq!(result["action"], "telegram_setup");
        assert_eq!(result["dryRun"], true);
        assert_eq!(result["telegram"]["configured"], true);
        assert_eq!(result["telegram"]["botToken"], "<redacted>");
        assert_eq!(result["config"]["telegram"]["botToken"], "<redacted>");
        assert_eq!(result["config"]["telegram"]["chatId"], "456");
        assert_eq!(result["config"]["telegram"]["allowedUserId"], "789");
        assert_eq!(result["daemonCommand"], "codex-telegram-bridge daemon run");
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("123:secret"),
            "setup output must not leak Telegram bot token"
        );
    }

    #[test]
    fn telegram_message_payload_contains_reply_buttons_for_approval_events() {
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_approval",
            "updatedAt": 42,
            "thread": {
                "displayName": "Approve deploy",
                "project": "infra",
                "pendingPrompt": {
                    "promptKind": "approval",
                    "question": "Deploy to production?"
                },
                "lastPreview": "Need approval"
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");

        assert_eq!(prepared.thread_id.as_deref(), Some("thr_approval"));
        assert_eq!(prepared.payloads.len(), 1);
        let payload = &prepared.payloads[0];
        assert!(payload["text"]
            .as_str()
            .expect("text")
            .contains("Approve deploy"));
        assert_eq!(payload["chat_id"], "999");
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["text"],
            "✅ Approve"
        );
        assert_eq!(
            prepared.callback_routes[0].action,
            TelegramCallbackAction::Approve
        );
        assert!(
            payload["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
                .as_str()
                .expect("callback")
                .len()
                <= 64,
            "Telegram callback_data must fit the Bot API limit"
        );
    }

    #[test]
    fn telegram_message_payload_preserves_full_multiline_codex_body() {
        let full_message =
            "\nFirst line from Codex.\n\nSecond line with details.\n- keep bullets\n- keep spacing\n";
        let event = json!({
            "type": "thread_completed",
            "threadId": "thr_done",
            "updatedAt": 42,
            "thread": {
                "displayName": "Finish release notes",
                "project": "codex-telegram-bridge",
                "lastPreview": full_message
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"].as_str().expect("text");

        assert!(text.contains("✅ Codex finished"));
        assert!(text.contains("🧵 Finish release notes"));
        assert!(text.contains("📁 codex-telegram-bridge"));
        assert!(text.contains(full_message));
    }

    #[test]
    fn telegram_message_payload_uses_remote_codex_chrome_only() {
        let full_message = "Done.\n\n::inbox-item{title=\"vault commands checked\" summary=\"No local Claude commands; LifeOS has four\"}";
        let event = json!({
            "type": "thread_completed",
            "threadId": "019d8f82-c4f5-7c00-a3ea-b0f118d8f2d",
            "updatedAt": 42,
            "thread": {
                "displayName": "LinkedIn Network",
                "lastPreview": full_message
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"].as_str().expect("text");

        assert!(text.starts_with("✅ Codex finished\n🧵 LinkedIn Network\n\n"));
        assert!(text.contains(full_message));
        assert!(text
            .contains("💬 To continue this thread, use Telegram's Reply action on this message."));
        assert!(!text.contains("🆔"));
        assert!(!text.contains("019d8f82-c4f5-7c00-a3ea-b0f118d8f2d"));
        assert!(!text.contains("📝 Codex"));
    }

    #[test]
    fn telegram_message_payload_shortens_app_file_reference_tokens() {
        let event = json!({
            "type": "thread_completed",
            "threadId": "thr_file_refs",
            "updatedAt": 42,
            "thread": {
                "displayName": "UI Experiment",
                "lastPreview": "Updated [F:/Users/hanifcarroll/projects/ui-experiment/README.md†L1-L24] and [F:/Users/hanifcarroll/projects/ui-experiment/src/styles.css†L70-L229]."
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"].as_str().expect("text");

        assert!(text.contains("README.md L1-L24"));
        assert!(text.contains("styles.css L70-L229"));
        assert!(!text.contains("[F:/Users/hanifcarroll/projects/ui-experiment/"));
    }

    #[test]
    fn telegram_approval_payload_uses_approval_title_and_button_footer() {
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_approval",
            "updatedAt": 42,
            "thread": {
                "displayName": "Deploy",
                "pendingPrompt": {
                    "promptKind": "approval",
                    "question": "Run `pnpm build`?"
                }
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"].as_str().expect("text");

        assert!(text.starts_with("🔐 Codex needs approval\n🧵 Deploy\n\n"));
        assert!(text.contains("Run `pnpm build`?"));
        assert!(
            text.contains("Use the buttons below, or use Telegram's Reply action on this message.")
        );
        assert_eq!(
            prepared.payloads[0]["reply_markup"]["inline_keyboard"][0][0]["text"],
            "✅ Approve"
        );
    }

    #[test]
    fn telegram_command_parser_supports_core_commands() {
        assert_eq!(
            parse_telegram_command_text("/away_on"),
            Some(TelegramInboundCommand::AwayOn)
        );
        assert_eq!(
            parse_telegram_command_text("/away_off@codex_bridge_bot"),
            Some(TelegramInboundCommand::AwayOff)
        );
        assert_eq!(
            parse_telegram_command_text("/new_thread Fix the formatter"),
            Some(TelegramInboundCommand::NewThread(Some(
                "Fix the formatter".to_string()
            )))
        );
        assert_eq!(
            parse_telegram_command_text("/new_thread"),
            Some(TelegramInboundCommand::NewThread(None))
        );
        assert_eq!(
            parse_telegram_command_text("/project bridge"),
            Some(TelegramInboundCommand::Project(Some("bridge".to_string())))
        );
        assert_eq!(
            parse_telegram_command_text("/project"),
            Some(TelegramInboundCommand::Project(None))
        );
        assert_eq!(
            parse_telegram_command_text("/projects"),
            Some(TelegramInboundCommand::Projects)
        );
        assert_eq!(
            parse_telegram_command_text("/unknown"),
            Some(TelegramInboundCommand::Unknown("/unknown".to_string()))
        );
    }

    #[test]
    fn telegram_command_extraction_requires_standalone_authorized_message() {
        let telegram = TelegramConfig {
            bot_token: "123:secret".to_string(),
            chat_id: "456".to_string(),
            allowed_user_id: Some("789".to_string()),
        };
        let command = extract_telegram_command(
            &json!({
                "chat": { "id": 456 },
                "from": { "id": 789 },
                "text": "/status"
            }),
            &telegram,
        )
        .expect("extract command");
        assert_eq!(command, Some(TelegramInboundCommand::Status));

        let reply_command = extract_telegram_command(
            &json!({
                "chat": { "id": 456 },
                "from": { "id": 789 },
                "text": "/away_on",
                "reply_to_message": { "message_id": 111 }
            }),
            &telegram,
        )
        .expect("extract reply command");
        assert_eq!(reply_command, None);

        let unauthorized = extract_telegram_command(
            &json!({
                "chat": { "id": 456 },
                "from": { "id": 111 },
                "text": "/status"
            }),
            &telegram,
        )
        .expect("extract unauthorized command");
        assert_eq!(unauthorized, None);
    }

    #[test]
    fn telegram_bot_commands_are_registered_for_core_remote_actions() {
        let commands = telegram_bot_commands();
        let names = commands
            .iter()
            .filter_map(|command| command.get("command").and_then(Value::as_str))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "start",
                "help",
                "away_on",
                "away_off",
                "status",
                "new_thread",
                "project",
                "projects",
                "inbox",
                "waiting",
                "recent",
                "settings"
            ]
        );
        for command in commands {
            assert!(
                command["command"].as_str().expect("command").len() <= 32,
                "Telegram command names must fit BotCommand limits"
            );
            assert!(!command["description"]
                .as_str()
                .expect("description")
                .is_empty());
        }
    }

    #[test]
    fn telegram_message_payload_splits_without_truncating_codex_body() {
        let full_message = "Codex line\n".repeat(500);
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_long",
            "updatedAt": 42,
            "thread": {
                "displayName": "Long update",
                "project": "codex-telegram-bridge",
                "lastPreview": full_message.clone()
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");

        assert!(prepared.payloads.len() > 1);
        for payload in &prepared.payloads {
            let text = payload["text"].as_str().expect("text");
            assert!(text.chars().count() <= TELEGRAM_MESSAGE_CHAR_LIMIT);
        }
        let joined = prepared
            .payloads
            .iter()
            .filter_map(|payload| payload["text"].as_str())
            .collect::<String>();
        assert!(joined.contains(&full_message));
    }

    #[test]
    fn daemon_install_dry_run_resolves_relative_bridge_command_for_services() {
        let result =
            install_daemon_service(DEFAULT_DAEMON_LABEL, "bin/codex-telegram-bridge", true)
                .expect("daemon install dry run");
        let expected = env::current_dir()
            .expect("cwd")
            .join("bin/codex-telegram-bridge")
            .display()
            .to_string();
        assert!(result["contents"]
            .as_str()
            .expect("service contents")
            .contains(&expected));
    }

    #[test]
    fn hermes_install_dry_run_builds_mcp_add_command() {
        let result = run_hermes_install(HermesInstallOptions {
            server_name: "codex",
            hermes_command: "hermes-se",
            bridge_command: "codex-telegram-bridge",
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
                "codex-telegram-bridge",
                "--args",
                "mcp"
            ])
        );
        assert_eq!(
            result["mcp"]["nextStep"],
            "Restart Hermes so it reconnects to MCP servers and discovers codex_* tools."
        );
        assert!(
            result.get("notificationLane").is_none(),
            "Hermes install should not configure Telegram notifications"
        );
    }

    #[test]
    fn outbound_events_dedupe_retry_and_deliver_durably() {
        let conn = create_state_db_in_memory().expect("db");
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_1",
            "updatedAt": 42
        });

        assert!(enqueue_outbound_event(&conn, &event, 1000).expect("enqueue"));
        assert!(
            !enqueue_outbound_event(&conn, &event, 1001).expect("dedupe"),
            "same delivery id should not enqueue twice"
        );

        let failed = deliver_due_outbound_events(&conn, 1000, 10, |_| bail!("Hermes offline"))
            .expect("failed delivery summary");
        assert_eq!(
            failed,
            OutboxDeliverySummary {
                attempted: 1,
                delivered: 0,
                failed: 1
            }
        );
        assert_eq!(pending_outbound_count(&conn).expect("pending"), 1);

        let delayed = deliver_due_outbound_events(&conn, 1000, 10, |_| Ok(json!({"ok": true})))
            .expect("not due summary");
        assert_eq!(delayed.attempted, 0);

        let delivered = deliver_due_outbound_events(&conn, 2000, 10, |_| Ok(json!({"ok": true})))
            .expect("delivered summary");
        assert_eq!(
            delivered,
            OutboxDeliverySummary {
                attempted: 1,
                delivered: 1,
                failed: 0
            }
        );
        assert_eq!(pending_outbound_count(&conn).expect("pending"), 0);
    }

    #[test]
    fn transport_delivery_log_tracks_each_transport_once() {
        let conn = create_state_db_in_memory().expect("db");

        assert!(
            !transport_delivery_exists(&conn, "event_1", "telegram").expect("lookup"),
            "transport should not start delivered"
        );
        record_transport_delivery(
            &conn,
            "event_1",
            "telegram",
            &json!({ "messageId": 111 }),
            1000,
        )
        .expect("record delivery");

        assert!(
            transport_delivery_exists(&conn, "event_1", "telegram").expect("lookup"),
            "recorded transport should be treated as delivered"
        );
        assert!(
            !transport_delivery_exists(&conn, "event_1", "hermes").expect("lookup"),
            "other transports for the same event must remain pending"
        );
    }

    #[test]
    fn daemon_notification_policy_only_enqueues_events_while_away() {
        let conn = create_state_db_in_memory().expect("db");
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_1",
            "updatedAt": 1500
        });

        let off_count =
            enqueue_daemon_notification_events(&conn, std::slice::from_ref(&event), 2000)
                .expect("away off enqueue");
        assert_eq!(
            off_count, 0,
            "daemon should stay quiet while user is present"
        );

        set_away_mode(&conn, true, 1000).expect("away on");
        let on_count =
            enqueue_daemon_notification_events(&conn, &[event], 2000).expect("away on enqueue");
        assert_eq!(on_count, 1, "daemon should notify while user is away");
    }

    #[test]
    fn daemon_notification_policy_skips_events_before_away_started() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 2000).expect("away on");
        let events = vec![
            json!({
                "type": "thread_waiting",
                "threadId": "thr_old",
                "updatedAt": 1500
            }),
            json!({
                "type": "thread_waiting",
                "threadId": "thr_new",
                "updatedAt": 2500
            }),
        ];

        let count = enqueue_daemon_notification_events(&conn, &events, 3000).expect("enqueue");

        assert_eq!(count, 1);
        assert_eq!(pending_outbound_count(&conn).expect("pending"), 1);
    }

    #[test]
    fn daemon_notification_policy_accepts_codex_second_timestamps() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 1_776_219_288_240).expect("away on");
        let events = vec![
            json!({
                "type": "thread_completed",
                "threadId": "thr_old",
                "updatedAt": 1_776_219_200
            }),
            json!({
                "type": "thread_completed",
                "threadId": "thr_new",
                "updatedAt": 1_776_219_396
            }),
        ];

        let count = enqueue_daemon_notification_events(&conn, &events, 1_776_219_397_000)
            .expect("mixed timestamp enqueue");

        assert_eq!(count, 1);
        assert_eq!(pending_outbound_count(&conn).expect("pending"), 1);
    }

    #[test]
    fn inbox_age_seconds_handles_mixed_timestamp_units() {
        let snapshot = snapshot_fixture(
            "thr_recent",
            "/tmp/project",
            1_776_219_396,
            "notLoaded",
            vec![],
            Some("completed"),
        );

        let item = classify_inbox_item(&snapshot, 1_776_219_400_000);

        assert_eq!(item.age_seconds, Some(4));
    }

    #[test]
    fn away_off_clears_pending_daemon_notifications() {
        let conn = create_state_db_in_memory().expect("db");
        set_away_mode(&conn, true, 1000).expect("away on");
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_1",
            "updatedAt": 1500
        });
        enqueue_daemon_notification_events(&conn, &[event], 2000).expect("enqueue");
        assert_eq!(pending_outbound_count(&conn).expect("pending"), 1);

        let disabled = set_away_mode(&conn, false, 2500).expect("away off");

        assert_eq!(disabled["away"], false);
        assert_eq!(disabled["clearedPendingNotifications"], 1);
        assert_eq!(pending_outbound_count(&conn).expect("pending"), 0);
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
            "codex-telegram-bridge"
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
            "codex_reply",
            "codex_approve",
        ] {
            assert!(names.contains(expected), "missing MCP tool {expected}");
        }
        for hidden in [
            "codex_watch",
            "codex_follow",
            "codex_sync",
            "codex_setup",
            "codex_daemon",
            "codex_telegram",
            "codex_new",
            "codex_fork",
            "codex_archive",
            "codex_unarchive",
            "codex_away",
            "codex_notify_away",
        ] {
            assert!(
                !names.contains(hidden),
                "{hidden} should stay out of the default Hermes MCP surface"
            );
        }

        let doctor = tools
            .iter()
            .find(|tool| tool["name"] == "codex_doctor")
            .expect("doctor tool");
        assert_eq!(doctor["annotations"]["readOnlyHint"], true);

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
    fn mcp_rejects_unlisted_control_tools() {
        for name in [
            "codex_new",
            "codex_fork",
            "codex_archive",
            "codex_unarchive",
            "codex_away",
            "codex_notify_away",
            "codex_watch",
            "codex_follow",
            "codex_sync",
            "codex_setup",
            "codex_daemon",
            "codex_telegram",
        ] {
            let response = mcp_handle_message(json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": name,
                    "arguments": {}
                }
            }))
            .expect("tools/call response");

            assert_eq!(response["result"]["isError"], true);
            assert!(response["result"]["structuredContent"]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Unknown MCP tool"));
        }
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

    #[test]
    fn telegram_new_thread_confirmation_reports_working_directory() {
        let message = telegram_new_thread_confirmation_text(
            &RegisteredProject {
                id: "ui-exp".to_string(),
                label: "UI Experiment".to_string(),
                cwd: "/Users/hanifcarroll/projects/ui-experiment".to_string(),
                aliases: Vec::new(),
            },
            &json!({
                "threadId": "thr_new",
                "cwd": "/Users/hanifcarroll/projects/ui-experiment"
            }),
        )
        .expect("message");

        assert!(message.contains("/Users/hanifcarroll/projects/ui-experiment"));
        assert!(message.contains("Use Telegram's Reply action on this message to continue it."));
    }
}
