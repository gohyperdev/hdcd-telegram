// SPDX-License-Identifier: Apache-2.0

//! Router configuration — reads `config.json` from the router state directory.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

use crate::token;

/// Router configuration loaded from
/// `~/.claude/channels/telegram-router/config.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterConfig {
    /// Bot token. Optional in `config.json` — if missing or empty the
    /// loader falls back to `HDCD_TELEGRAM_BOT_TOKEN` /
    /// `TELEGRAM_BOT_TOKEN` env vars, then to the standalone channel's
    /// `~/.claude/channels/telegram/.env` file. Always populated after
    /// `load()` returns.
    #[serde(default)]
    pub bot_token: String,
    /// Supergroup chat ID. Accepts both JSON integer (`-1001234567890`)
    /// and JSON string (`"-1001234567890"`) on deserialization so users
    /// can copy the id however their tools emit it.
    #[serde(deserialize_with = "de_chat_id")]
    pub supergroup_id: i64,
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

impl RouterConfig {
    /// String form of `supergroup_id` for Telegram API calls that take
    /// `chat_id: &str`.
    pub fn chat_id_str(&self) -> String {
        self.supergroup_id.to_string()
    }
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

/// Accept either a JSON integer or a JSON string for the chat id.
/// Strings are trimmed before parsing to tolerate leading/trailing
/// whitespace from copy-paste.
fn de_chat_id<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = i64;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a Telegram chat id (integer or string, e.g. -1001234567890)")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<i64, E> {
            Ok(v)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<i64, E> {
            i64::try_from(v).map_err(|_| E::custom("chat id overflows i64"))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<i64, E> {
            v.trim().parse::<i64>().map_err(|e| {
                E::custom(format!(
                    "invalid chat id {:?}: {} (expected integer like -1001234567890)",
                    v, e
                ))
            })
        }
    }
    d.deserialize_any(V)
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
    let _ = crate::fs_perms::secure_dir(&dir);
    Ok(dir)
}

/// Load router config from `config.json` in the given state directory.
pub fn load(state_dir: &Path) -> Result<RouterConfig> {
    let path = state_dir.join("config.json");
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut config: RouterConfig = serde_json::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;

    if config.bot_token.is_empty() {
        let fallback_dir = standalone_state_dir()?;
        match token::try_load_token(&fallback_dir)? {
            Some(t) => config.bot_token = t,
            None => anyhow::bail!(
                "bot_token required — set one of:\n  \
                 - `bot_token` field in {}\n  \
                 - `HDCD_TELEGRAM_BOT_TOKEN` env var (preferred)\n  \
                 - `TELEGRAM_BOT_TOKEN` env var (legacy)\n  \
                 - `HDCD_TELEGRAM_BOT_TOKEN=...` line in {}",
                path.display(),
                fallback_dir.join(".env").display()
            ),
        }
    }
    if config.supergroup_id == 0 {
        anyhow::bail!(
            "supergroup_id is 0 in {} (expected a Telegram chat id like -1001234567890)",
            path.display()
        );
    }

    Ok(config)
}

/// Standalone channel's state dir (`~/.claude/channels/telegram/`), used
/// for the `.env` fallback when `config.json` omits `bot_token`.
/// Honors `TELEGRAM_STATE_DIR` the same way the standalone binary does,
/// so a user who overrides one also overrides the other.
fn standalone_state_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("TELEGRAM_STATE_DIR") {
        return Ok(PathBuf::from(d));
    }
    Ok(dirs::home_dir()
        .context("home dir unavailable")?
        .join(".claude")
        .join("channels")
        .join("telegram"))
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
        assert_eq!(config.supergroup_id, -1001234567890);
        assert_eq!(config.chat_id_str(), "-1001234567890");
        assert!(config.allowed_users.is_empty());
        assert!(config.close_topic_on_disconnect);
        assert_eq!(config.outbox_poll_interval_ms, 200);
        assert_eq!(config.inbox_poll_interval_ms, 500);
        assert_eq!(config.health_check_interval_s, 30);
        assert_eq!(config.auto_shutdown_delay_s, 60);
    }

    #[test]
    fn supergroup_id_accepts_integer() {
        let json = r#"{
            "bot_token": "123:AAHtest",
            "supergroup_id": -1001234567890
        }"#;
        let config: RouterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.supergroup_id, -1001234567890);
    }

    #[test]
    fn supergroup_id_trims_whitespace() {
        let json = r#"{
            "bot_token": "123:AAHtest",
            "supergroup_id": "  -1001234567890  "
        }"#;
        let config: RouterConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.supergroup_id, -1001234567890);
    }

    #[test]
    fn supergroup_id_invalid_string_rejected() {
        let json = r#"{
            "bot_token": "123:AAHtest",
            "supergroup_id": "not-a-number"
        }"#;
        let err = serde_json::from_str::<RouterConfig>(json).unwrap_err().to_string();
        assert!(err.contains("invalid chat id"), "got: {err}");
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

    use crate::token::with_isolated_token_env;

    #[test]
    fn empty_token_with_no_fallback_rejected() {
        with_isolated_token_env(|| {
            let dir = tempfile::tempdir().unwrap();
            let json = r#"{"bot_token": "", "supergroup_id": "-1001234567890"}"#;
            std::fs::write(dir.path().join("config.json"), json).unwrap();
            let err = load(dir.path()).unwrap_err().to_string();
            assert!(err.contains("bot_token required"), "got: {err}");
        });
    }

    #[test]
    fn missing_bot_token_field_uses_env_fallback() {
        with_isolated_token_env(|| {
            std::env::set_var("HDCD_TELEGRAM_BOT_TOKEN", "env-fallback-token");
            let dir = tempfile::tempdir().unwrap();
            let json = r#"{"supergroup_id": "-1001234567890"}"#;
            std::fs::write(dir.path().join("config.json"), json).unwrap();
            let config = load(dir.path()).unwrap();
            assert_eq!(config.bot_token, "env-fallback-token");
        });
    }

    #[test]
    fn zero_supergroup_rejected_by_load() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"bot_token": "x", "supergroup_id": 0}"#;
        std::fs::write(dir.path().join("config.json"), json).unwrap();
        let err = load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("supergroup_id is 0"), "got: {err}");
    }
}
