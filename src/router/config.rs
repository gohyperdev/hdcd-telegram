// SPDX-License-Identifier: Apache-2.0

//! Router configuration — reads `config.json` from the router state directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Router configuration loaded from
/// `~/.claude/channels/telegram-router/config.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    pub bot_token: String,
    pub supergroup_id: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default = "default_session_label_format")]
    pub session_label_format: String,
    #[serde(default = "default_close_on_disconnect")]
    pub close_topic_on_disconnect: bool,
    #[serde(default = "default_inbox_poll_ms")]
    pub inbox_poll_interval_ms: u64,
    #[serde(default = "default_outbox_poll_ms")]
    pub outbox_poll_interval_ms: u64,
    #[serde(default = "default_health_check_s")]
    pub health_check_interval_s: u64,
    #[serde(default = "default_auto_shutdown_s")]
    pub auto_shutdown_delay_s: u64,
}

fn default_session_label_format() -> String {
    "{cwd_basename}".into()
}
fn default_close_on_disconnect() -> bool {
    true
}
fn default_inbox_poll_ms() -> u64 {
    500
}
fn default_outbox_poll_ms() -> u64 {
    200
}
fn default_health_check_s() -> u64 {
    30
}
fn default_auto_shutdown_s() -> u64 {
    60
}

/// Resolve the router state directory: `~/.claude/channels/telegram-router/`.
pub fn state_dir() -> Result<PathBuf> {
    let dir = if let Ok(d) = std::env::var("ROUTER_STATE_DIR") {
        PathBuf::from(d)
    } else {
        dirs::home_dir()
            .context("home dir unavailable")?
            .join(".claude")
            .join("channels")
            .join("telegram-router")
    };
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create router state dir {}", dir.display()))?;
    Ok(dir)
}

/// Load router config from `config.json` in the given state directory.
pub fn load(state_dir: &Path) -> Result<RouterConfig> {
    let path = state_dir.join("config.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let config: RouterConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;

    if config.bot_token.is_empty() {
        anyhow::bail!("bot_token is empty in {}", path.display());
    }
    if config.supergroup_id.is_empty() {
        anyhow::bail!("supergroup_id is empty in {}", path.display());
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_parses() {
        let json = r#"{
            "bot_token": "123:AAHtest",
            "supergroup_id": "-1001234567890"
        }"#;
        let config: RouterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.bot_token, "123:AAHtest");
        assert_eq!(config.supergroup_id, "-1001234567890");
        assert!(config.allowed_users.is_empty());
        assert!(config.close_topic_on_disconnect);
        assert_eq!(config.outbox_poll_interval_ms, 200);
        assert_eq!(config.inbox_poll_interval_ms, 500);
        assert_eq!(config.health_check_interval_s, 30);
        assert_eq!(config.auto_shutdown_delay_s, 60);
    }

    #[test]
    fn full_config_parses() {
        let json = r#"{
            "bot_token": "123:AAHtest",
            "supergroup_id": "-1001234567890",
            "allowed_users": ["123456789"],
            "session_label_format": "{cwd_basename}",
            "close_topic_on_disconnect": false,
            "inbox_poll_interval_ms": 1000,
            "outbox_poll_interval_ms": 100,
            "health_check_interval_s": 60,
            "auto_shutdown_delay_s": 0
        }"#;
        let config: RouterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.allowed_users, vec!["123456789"]);
        assert!(!config.close_topic_on_disconnect);
        assert_eq!(config.inbox_poll_interval_ms, 1000);
        assert_eq!(config.auto_shutdown_delay_s, 0);
    }

    #[test]
    fn empty_token_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"bot_token": "", "supergroup_id": "-100"}"#;
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        assert!(load(dir.path()).is_err());
    }
}
