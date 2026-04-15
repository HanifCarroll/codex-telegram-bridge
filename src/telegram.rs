mod api;
mod render;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::thread;
use std::time::Duration;

use crate::codex::{
    normalized_message, set_away_mode, start_thread_in_cwd, text_input_value,
    wait_for_started_turn, CodexAppServerClient, TELEGRAM_TURN_SETTLE_POLL_MS,
    TELEGRAM_TURN_SETTLE_TIMEOUT_MS,
};
use crate::projects::{resolve_new_thread_request, resolve_project_query};
use crate::state::{
    get_setting_number, get_telegram_current_project_id, insert_telegram_callback_route,
    insert_telegram_command_route, insert_telegram_message_route, lookup_telegram_command_route,
    lookup_telegram_message_route, mark_telegram_command_route_used, observed_workspaces_from_db,
    record_action, record_telegram_inbound_processed, set_setting, set_telegram_current_project_id,
    telegram_inbound_processed, update_telegram_callback_message_id, TelegramCallbackAction,
    TelegramCommandRouteKind,
};
use crate::{
    daemon_config_path, load_daemon_config, merged_daemon_config, read_daemon_config_raw,
    redacted_daemon_config, resolve_telegram_bot_token, write_daemon_config, DaemonConfig,
    RegisteredProject, TelegramConfig, TelegramSetupOptions,
};

use self::api::{
    telegram_answer_callback_query, telegram_bot_commands, telegram_chat_id,
    telegram_delete_webhook, telegram_from_user_id, telegram_get_updates, telegram_message_id,
    telegram_send_message, telegram_send_text, telegram_send_text_message_id,
    telegram_updates_array,
};
use self::render::{
    prepare_telegram_delivery, telegram_help_text, telegram_inbox_text,
    telegram_new_thread_confirmation_text, telegram_project_text, telegram_projects_text,
    telegram_recent_text, telegram_settings_text, telegram_status_text, telegram_waiting_text,
};

pub(crate) use self::api::{telegram_bot_id, telegram_set_my_commands};

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
pub(crate) struct RoutedTelegramCommandPromptReply {
    pub(crate) kind: TelegramCommandRouteKind,
    pub(crate) message: String,
    pub(crate) project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutedTelegramReply {
    pub(crate) thread_id: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoutedTelegramCallback {
    pub(crate) callback_query_id: String,
    pub(crate) thread_id: String,
    pub(crate) action: TelegramCallbackAction,
}

pub(crate) fn telegram_setup_result(options: TelegramSetupOptions<'_>) -> Result<Value> {
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
        "daemonCommand": crate::daemon_run_command(bridge_command),
        "daemonInstallCommand": format!(
            "{} daemon install --bridge-command {}",
            crate::shell_quote(bridge_command),
            crate::shell_quote(bridge_command)
        ),
        "nextStep": "Install and start the daemon. Codex updates will go directly to Telegram, Telegram replies route back to the originating thread, and slash commands control away mode or start new threads."
    }))
}

pub(crate) fn telegram_status_result() -> Result<Value> {
    let config = read_daemon_config_raw()?;
    Ok(json!({
        "ok": true,
        "action": "telegram_status",
        "configPath": daemon_config_path()?.display().to_string(),
        "configured": config.as_ref().and_then(|config| config.telegram.as_ref()).is_some(),
        "config": config.as_ref().map(redacted_daemon_config)
    }))
}

pub(crate) fn telegram_test_result(
    message: &str,
    timeout: Duration,
    dry_run: bool,
) -> Result<Value> {
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

pub(crate) fn telegram_disable_result(dry_run: bool) -> Result<Value> {
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
        "removedConfig": removes_config,
        "configPath": path.display().to_string()
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

pub(crate) fn extract_telegram_reply_route(
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

pub(crate) fn extract_telegram_command_prompt_reply(
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

pub(crate) fn extract_telegram_callback_route(
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

pub(crate) fn deliver_telegram_event(
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
        thread_id,
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
            Ok(json!({
                "ok": true,
                "action": "telegram_unknown_command",
                "command": command,
                "sent": sent
            }))
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

pub(crate) fn process_telegram_updates(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_setup_dry_run_writes_redacted_daemon_shape() {
        let result = telegram_setup_result(TelegramSetupOptions {
            bot_token: Some("123:secret"),
            chat_id: Some("456"),
            allowed_user_id: Some("789"),
            events: crate::DEFAULT_NOTIFICATION_EVENTS,
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
                "chat": { "id": "456" },
                "from": { "id": "789" },
                "text": "/status"
            }),
            &telegram,
        )
        .expect("extract command");
        assert_eq!(command, Some(TelegramInboundCommand::Status));

        let reply_command = extract_telegram_command(
            &json!({
                "chat": { "id": "456" },
                "from": { "id": "789" },
                "text": "/status",
                "reply_to_message": { "message_id": 1 }
            }),
            &telegram,
        )
        .expect("extract reply command");
        assert_eq!(reply_command, None);

        let unauthorized = extract_telegram_command(
            &json!({
                "chat": { "id": "999" },
                "from": { "id": "789" },
                "text": "/status"
            }),
            &telegram,
        )
        .expect("extract unauthorized command");
        assert_eq!(unauthorized, None);
    }
}
