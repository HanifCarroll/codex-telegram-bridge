use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::PathBuf;

use crate::state_dir_path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonConfig {
    pub(crate) version: u32,
    pub(crate) bridge_command: String,
    pub(crate) events: String,
    #[serde(default)]
    pub(crate) telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub(crate) projects: Vec<RegisteredProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TelegramConfig {
    pub(crate) bot_token: String,
    pub(crate) chat_id: String,
    #[serde(default)]
    pub(crate) allowed_user_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegisteredProject {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) cwd: String,
    #[serde(default)]
    pub(crate) aliases: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TelegramSetupOptions<'a> {
    pub(crate) bot_token: Option<&'a str>,
    pub(crate) chat_id: Option<&'a str>,
    pub(crate) allowed_user_id: Option<&'a str>,
    pub(crate) events: &'a str,
    pub(crate) bridge_command: &'a str,
    pub(crate) dry_run: bool,
    pub(crate) pair_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SetupOptions<'a> {
    pub(crate) bot_token: Option<&'a str>,
    pub(crate) chat_id: Option<&'a str>,
    pub(crate) allowed_user_id: Option<&'a str>,
    pub(crate) events: &'a str,
    pub(crate) bridge_command: &'a str,
    pub(crate) daemon_label: &'a str,
    pub(crate) install_daemon: bool,
    pub(crate) start_daemon: bool,
    pub(crate) register_hermes: bool,
    pub(crate) hermes_server_name: &'a str,
    pub(crate) hermes_command: &'a str,
    pub(crate) dry_run: bool,
    pub(crate) pair_timeout_ms: u64,
}

fn daemon_config_path() -> Result<PathBuf> {
    Ok(state_dir_path()?.join("config.json"))
}

pub(crate) fn merged_daemon_config(
    existing: Option<&DaemonConfig>,
    bridge_command: &str,
    events: &str,
    telegram: TelegramConfig,
) -> DaemonConfig {
    DaemonConfig {
        version: 3,
        bridge_command: bridge_command.to_string(),
        events: events.to_string(),
        telegram: Some(telegram),
        projects: existing
            .map(|config| config.projects.clone())
            .unwrap_or_default(),
    }
}

pub(crate) fn write_daemon_config(config: &DaemonConfig) -> Result<PathBuf> {
    let path = daemon_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

pub(crate) fn load_daemon_config() -> Result<DaemonConfig> {
    let path = daemon_config_path()?;
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("daemon config not found at {}", path.display()))?;
    let config: DaemonConfig = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse daemon config at {}", path.display()))?;
    if config.events.trim().is_empty() {
        bail!("daemon config events cannot be empty");
    }
    match config.telegram.as_ref() {
        Some(telegram) => {
            if telegram.bot_token.trim().is_empty() {
                bail!("daemon config telegram.botToken cannot be empty");
            }
            if telegram.chat_id.trim().is_empty() {
                bail!("daemon config telegram.chatId cannot be empty");
            }
        }
        None => bail!(
            "daemon config must include Telegram transport. Run `codex-telegram-bridge setup` or `codex-telegram-bridge telegram setup`."
        ),
    }
    for project in &config.projects {
        if project.id.trim().is_empty() {
            bail!("daemon config project id cannot be empty");
        }
        if project.label.trim().is_empty() {
            bail!("daemon config project label cannot be empty");
        }
        if project.cwd.trim().is_empty() {
            bail!("daemon config project cwd cannot be empty");
        }
    }
    Ok(config)
}

pub(crate) fn redacted_daemon_config(config: &DaemonConfig) -> Value {
    json!({
        "version": config.version,
        "bridgeCommand": config.bridge_command,
        "events": config.events,
        "telegram": config.telegram.as_ref().map(|telegram| json!({
            "botToken": "<redacted>",
            "chatId": telegram.chat_id,
            "allowedUserId": telegram.allowed_user_id
        })),
        "projects": config.projects.iter().map(|project| json!({
            "id": project.id,
            "label": project.label,
            "cwd": project.cwd,
            "aliases": project.aliases
        })).collect::<Vec<_>>()
    })
}

pub(crate) fn read_daemon_config_raw() -> Result<Option<DaemonConfig>> {
    let path = daemon_config_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read daemon config at {}", path.display()))?;
    let config: DaemonConfig = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse daemon config at {}", path.display()))?;
    Ok(Some(config))
}

pub(crate) fn resolve_telegram_bot_token(explicit: Option<&str>) -> Result<String> {
    explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            env::var("TELEGRAM_BOT_TOKEN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .context(
            "Telegram bot token is required. Pass --bot-token or set TELEGRAM_BOT_TOKEN after creating a bot with @BotFather.",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merged_daemon_config_preserves_existing_projects() {
        let merged = merged_daemon_config(
            Some(&DaemonConfig {
                version: 2,
                bridge_command: "old-bridge".to_string(),
                events: "thread_waiting".to_string(),
                telegram: Some(TelegramConfig {
                    bot_token: "old".to_string(),
                    chat_id: "old-chat".to_string(),
                    allowed_user_id: None,
                }),
                projects: vec![RegisteredProject {
                    id: "bridge".to_string(),
                    label: "Codex Telegram Bridge".to_string(),
                    cwd: "/Users/hanifcarroll/projects/codex-telegram-bridge".to_string(),
                    aliases: vec!["codex".to_string()],
                }],
            }),
            "codex-telegram-bridge",
            crate::DEFAULT_NOTIFICATION_EVENTS,
            TelegramConfig {
                bot_token: "123:secret".to_string(),
                chat_id: "456".to_string(),
                allowed_user_id: Some("789".to_string()),
            },
        );

        assert_eq!(merged.version, 3);
        assert_eq!(merged.projects.len(), 1);
        assert_eq!(merged.projects[0].id, "bridge");
        assert_eq!(merged.telegram.as_ref().unwrap().chat_id, "456");
    }
}
