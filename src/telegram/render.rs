use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

use crate::codex::get_away_mode;
use crate::config::{DaemonConfig, RegisteredProject, TelegramConfig};
use crate::projects::derive_project_label;
use crate::state::{
    derive_thread_display_name, list_inbox_from_db, list_waiting_from_db, pending_outbound_count,
    ObservedWorkspace, TelegramCallbackAction, TelegramCallbackRoute,
};

const TELEGRAM_CONTINUE_THREAD_HINT: &str =
    "💬 To continue this thread, use Telegram's Reply action on this message.";
const TELEGRAM_ANSWER_THREAD_HINT: &str =
    "💬 To answer Codex, use Telegram's Reply action on this message.";
const TELEGRAM_APPROVAL_HINT: &str =
    "Use the buttons below, or use Telegram's Reply action on this message.";
const TELEGRAM_MESSAGE_CHAR_LIMIT: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedTelegramDelivery {
    pub(crate) payloads: Vec<Value>,
    pub(crate) thread_id: Option<String>,
    pub(crate) event_id: String,
    pub(crate) callback_routes: Vec<TelegramCallbackRoute>,
}

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
    let digest = crate::sha256_hex(format!("{event_id}:{}", action.as_str()).as_bytes());
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

pub(crate) fn prepare_telegram_delivery(
    chat_id: &str,
    event: &Value,
) -> Result<PreparedTelegramDelivery> {
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("codex_event");
    let event_id = crate::notification_event_id(event);
    let thread_id = crate::event_thread_id(event);
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

pub(crate) fn telegram_projects_text(
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

pub(crate) fn telegram_project_text(project: Option<&RegisteredProject>) -> String {
    match project {
        Some(project) => format!(
            "Current project\n\n{} ({})\n{}\n\nNew Telegram threads will start here until you switch again.",
            project.id, project.label, project.cwd
        ),
        None => "No current project is selected.\n\nUse /projects to inspect the registry, then /project <id> to choose one."
            .to_string(),
    }
}

pub(crate) fn telegram_help_text() -> String {
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

pub(crate) fn telegram_status_text(conn: &Connection) -> Result<String> {
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

pub(crate) fn telegram_settings_text(
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

pub(crate) fn telegram_waiting_text(conn: &Connection) -> Result<String> {
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

pub(crate) fn telegram_inbox_text(conn: &Connection, now: u64) -> Result<String> {
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

pub(crate) fn telegram_recent_text(conn: &Connection) -> Result<String> {
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

pub(crate) fn telegram_new_thread_confirmation_text(
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(prepared.callback_routes.len(), 2);
        let reply_markup = prepared.payloads[0]["reply_markup"]["inline_keyboard"]
            .as_array()
            .expect("inline keyboard");
        assert_eq!(reply_markup.len(), 1);
        let buttons = reply_markup[0].as_array().expect("buttons");
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["text"], "✅ Approve");
        assert_eq!(buttons[1]["text"], "🛑 Deny");
        assert_eq!(
            prepared.callback_routes[0].action,
            TelegramCallbackAction::Approve
        );
        for button in buttons {
            let callback_data = button["callback_data"].as_str().expect("callback data");
            assert!(
                callback_data.len() <= 64,
                "Telegram callback_data must fit the Bot API limit"
            );
        }
    }

    #[test]
    fn telegram_message_payload_preserves_full_multiline_codex_body() {
        let full_message =
            "Done.\n\nFirst line stays.\nSecond line also stays.\n\nThird paragraph remains intact.";
        let event = json!({
            "type": "thread_completed",
            "threadId": "thr_done",
            "updatedAt": 42,
            "thread": {
                "displayName": "LinkedIn Network",
                "project": "growth",
                "lastPreview": full_message
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"]
            .as_str()
            .expect("telegram text");
        assert!(text.contains(full_message));
        assert!(!text.contains("ID "));
        assert!(!text.contains("\n🤖 Codex\n"));
    }

    #[test]
    fn telegram_message_payload_uses_remote_codex_chrome_only() {
        let full_message = "Done.\n\n::inbox-item{title=\"vault commands checked\" summary=\"No local Claude commands; LifeOS has four\"}";
        let event = json!({
            "type": "thread_completed",
            "threadId": "thr_done",
            "updatedAt": 42,
            "thread": {
                "displayName": "Vault commands checked",
                "project": "ops",
                "lastPreview": full_message
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"]
            .as_str()
            .expect("telegram text");
        assert!(text.starts_with("✅ Codex finished\n🧵 Vault commands checked\n📁 ops"));
        assert!(!text.contains("🤖 Codex"));
        assert!(!text.contains("ID "));
        assert!(text.contains("::inbox-item"));
        assert!(text
            .contains("💬 To continue this thread, use Telegram's Reply action on this message."));
    }

    #[test]
    fn telegram_message_payload_shortens_app_file_reference_tokens() {
        let preview = "Updated the docs. [F:/Users/hanifcarroll/projects/ui-experiment/README.md†L1-L24] [F:/Users/hanifcarroll/projects/ui-experiment/docs/airbnb-design-implementation.md†L1-L19]";
        let event = json!({
            "type": "thread_completed",
            "threadId": "thr_docs",
            "updatedAt": 42,
            "thread": {
                "displayName": "Docs updated",
                "project": "ui-exp",
                "lastPreview": preview
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"]
            .as_str()
            .expect("telegram text");
        assert!(text.contains("README.md L1-L24"));
        assert!(text.contains("airbnb-design-implementation.md L1-L19"));
        assert!(!text.contains("[F:/Users/hanifcarroll/projects/ui-experiment/README.md"));
    }

    #[test]
    fn telegram_approval_payload_uses_approval_title_and_button_footer() {
        let event = json!({
            "type": "thread_waiting",
            "threadId": "thr_approval",
            "updatedAt": 42,
            "thread": {
                "displayName": "Deploy request",
                "project": "infra",
                "pendingPrompt": {
                    "kind": "approval",
                    "question": "Ship the hotfix?"
                }
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        let text = prepared.payloads[0]["text"]
            .as_str()
            .expect("telegram text");
        assert!(text.starts_with("🔐 Codex needs approval"));
        assert!(text.contains("Ship the hotfix?"));
        assert!(
            text.contains("Use the buttons below, or use Telegram's Reply action on this message.")
        );
    }

    #[test]
    fn telegram_message_payload_splits_without_truncating_codex_body() {
        let long_preview = "x".repeat(10_000);
        let event = json!({
            "type": "thread_completed",
            "threadId": "thr_split",
            "updatedAt": 42,
            "thread": {
                "displayName": "Long answer",
                "project": "ops",
                "lastPreview": long_preview
            }
        });

        let prepared = prepare_telegram_delivery("999", &event).expect("prepared telegram event");
        assert!(
            prepared.payloads.len() > 1,
            "long telegram messages should split"
        );
        for payload in &prepared.payloads {
            let text = payload["text"].as_str().expect("telegram text");
            assert!(text.chars().count() <= TELEGRAM_MESSAGE_CHAR_LIMIT);
        }
        let combined = prepared
            .payloads
            .iter()
            .map(|payload| payload["text"].as_str().expect("telegram text"))
            .collect::<Vec<_>>()
            .join("");
        assert!(combined.contains("Long answer"));
        assert!(combined.contains("💬 To continue this thread"));
        assert!(combined.contains(&"x".repeat(5000)));
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
                "threadId": "thr_123",
                "cwd": "/Users/hanifcarroll/projects/ui-experiment"
            }),
        )
        .expect("confirmation text");

        assert!(message.contains("Started a new Codex thread in UI Experiment."));
        assert!(message.contains("/Users/hanifcarroll/projects/ui-experiment"));
        assert!(message.contains("Use Telegram's Reply action on this message to continue it."));
    }
}
