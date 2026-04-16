use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::TelegramConfig;

pub(crate) fn telegram_api_post(
    bot_token: &str,
    method: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<Value> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let url = format!(
        "https://api.telegram.org/bot{}/{}",
        bot_token.trim(),
        method.trim()
    );
    let response = agent
        .post(&url)
        .send_json(payload.clone())
        .map_err(|error| {
            anyhow!(
                "Telegram API {method} request failed: {}",
                crate::redact_secret_text(&error.to_string(), bot_token)
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

pub(crate) fn telegram_delete_webhook(bot_token: &str, timeout: Duration) -> Result<Value> {
    telegram_api_post(
        bot_token,
        "deleteWebhook",
        &json!({ "drop_pending_updates": false }),
        timeout,
    )
}

pub(crate) fn telegram_get_updates(
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

pub(crate) fn telegram_send_message(
    telegram: &TelegramConfig,
    payload: &Value,
    timeout: Duration,
) -> Result<Value> {
    telegram_api_post(&telegram.bot_token, "sendMessage", payload, timeout)
}

pub(crate) fn telegram_send_text(
    telegram: &TelegramConfig,
    text: &str,
    timeout: Duration,
) -> Result<Value> {
    telegram_send_message(
        telegram,
        &json!({
            "chat_id": telegram.chat_id,
            "text": text,
            "disable_web_page_preview": true
        }),
        timeout,
    )
}

pub(crate) fn telegram_send_text_message_id(
    telegram: &TelegramConfig,
    text: &str,
    timeout: Duration,
) -> Result<i64> {
    telegram_send_text(telegram, text, timeout)?
        .pointer("/result/message_id")
        .and_then(Value::as_i64)
        .context("Telegram sendMessage response missing result.message_id")
}

pub(crate) fn telegram_bot_commands() -> Vec<Value> {
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

pub(crate) fn telegram_set_my_commands(
    telegram: &TelegramConfig,
    timeout: Duration,
) -> Result<Value> {
    telegram_api_post(
        &telegram.bot_token,
        "setMyCommands",
        &json!({ "commands": telegram_bot_commands() }),
        timeout,
    )
}

pub(crate) fn telegram_answer_callback_query(
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

pub(crate) fn telegram_updates_array(updates: &Value) -> Result<&[Value]> {
    updates
        .get("result")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .context("Telegram getUpdates response did not contain result array")
}

pub(crate) fn telegram_chat_id(message: &Value) -> Option<String> {
    message
        .get("chat")
        .and_then(|chat| chat.get("id"))
        .and_then(|value| {
            value
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| value.as_str().map(str::to_string))
        })
}

pub(crate) fn telegram_from_user_id(message: &Value) -> Option<String> {
    message
        .get("from")
        .and_then(|from| from.get("id"))
        .and_then(|value| {
            value
                .as_i64()
                .map(|id| id.to_string())
                .or_else(|| value.as_str().map(str::to_string))
        })
}

pub(crate) fn telegram_message_id(message: &Value) -> Option<i64> {
    message.get("message_id").and_then(Value::as_i64)
}

pub(crate) fn telegram_bot_id(bot_token: &str) -> String {
    crate::sha256_hex(bot_token.as_bytes())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_bot_commands_are_registered_for_core_remote_actions() {
        let commands = telegram_bot_commands();
        let names = commands
            .iter()
            .filter_map(|command| command.get("command").and_then(Value::as_str))
            .collect::<Vec<_>>();

        for required in [
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
            "settings",
        ] {
            assert!(
                names.contains(&required),
                "missing required telegram bot command {required}"
            );
        }
        for command in commands {
            assert!(
                command["command"].as_str().expect("command").len() <= 32,
                "Telegram command names must fit BotCommand limits"
            );
            assert!(
                !command["description"]
                    .as_str()
                    .expect("description")
                    .trim()
                    .is_empty(),
                "Telegram commands must include human-readable descriptions"
            );
        }
    }
}
