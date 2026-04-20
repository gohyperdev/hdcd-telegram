// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Bot token discovery shared by standalone and router modes.
//!
//! Priority order:
//! 1. `HDCD_TELEGRAM_BOT_TOKEN` env var (preferred, avoids conflict with official plugin)
//! 2. `TELEGRAM_BOT_TOKEN` env var (backward compatible)
//! 3. `HDCD_TELEGRAM_BOT_TOKEN=` line in `<state_dir>/.env`
//! 4. `TELEGRAM_BOT_TOKEN=` line in `<state_dir>/.env` (backward compatible)

use std::path::Path;

use anyhow::{Context, Result};

/// Try to discover a bot token. Returns `Ok(None)` when nothing is
/// configured, `Ok(Some(token))` on success, and `Err` only for
/// unexpected I/O failures while reading `.env`.
pub fn try_load_token(state_dir: &Path) -> Result<Option<String>> {
    for var_name in ["HDCD_TELEGRAM_BOT_TOKEN", "TELEGRAM_BOT_TOKEN"] {
        if let Ok(token) = std::env::var(var_name) {
            if !token.is_empty() {
                return Ok(Some(token));
            }
        }
    }

    let env_file = state_dir.join(".env");
    if env_file.exists() {
        warn_if_world_readable(&env_file);

        let content = std::fs::read_to_string(&env_file)
            .with_context(|| format!("read {}", env_file.display()))?;
        for prefix in ["HDCD_TELEGRAM_BOT_TOKEN=", "TELEGRAM_BOT_TOKEN="] {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix(prefix) {
                    let trimmed = rest.trim();
                    let token = trimmed
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                        .or_else(|| {
                            trimmed
                                .strip_prefix('\'')
                                .and_then(|s| s.strip_suffix('\''))
                        })
                        .unwrap_or(trimmed)
                        .to_string();
                    if !token.is_empty() {
                        return Ok(Some(token));
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Load the bot token or fail with a descriptive error pointing to the
/// state dir's `.env` path. Used by standalone mode, where a token is
/// mandatory.
pub fn load_token(state_dir: &Path) -> Result<String> {
    match try_load_token(state_dir)? {
        Some(t) => Ok(t),
        None => anyhow::bail!(
            "Bot token required — set HDCD_TELEGRAM_BOT_TOKEN (or TELEGRAM_BOT_TOKEN for backward compatibility)\n  \
             in env or {}\n  \
             format: HDCD_TELEGRAM_BOT_TOKEN=123456789:AAH...",
            state_dir.join(".env").display()
        ),
    }
}

fn warn_if_world_readable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode();
            if mode & 0o044 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    "WARNING: {} is world-readable, consider: chmod 600 {}",
                    path.display(),
                    path.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Serializes env-var access across token-discovery tests across the
/// whole crate. Env vars are process-global; without a single shared
/// mutex, parallel tests in different modules race.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with `HDCD_TELEGRAM_BOT_TOKEN` / `TELEGRAM_BOT_TOKEN` /
/// `HOME` / `USERPROFILE` cleared and `HOME`/`USERPROFILE` pointed at
/// a fresh empty dir. Restores previous values on exit. Used by every
/// test that exercises token discovery (here and in `router::config`).
#[cfg(test)]
pub(crate) fn with_isolated_token_env<F: FnOnce()>(f: F) {
    let _g = TEST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev_hdcd = std::env::var("HDCD_TELEGRAM_BOT_TOKEN").ok();
    let prev_tg = std::env::var("TELEGRAM_BOT_TOKEN").ok();
    let prev_state_dir = std::env::var("TELEGRAM_STATE_DIR").ok();
    std::env::remove_var("HDCD_TELEGRAM_BOT_TOKEN");
    std::env::remove_var("TELEGRAM_BOT_TOKEN");
    // Redirect the standalone state dir — which holds `.env` — to an
    // empty tempdir so `token::try_load_token` can't find a real token
    // on the developer's machine.
    let fake_state = tempfile::tempdir().expect("tempdir for fake TELEGRAM_STATE_DIR");
    std::env::set_var("TELEGRAM_STATE_DIR", fake_state.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    match prev_hdcd {
        Some(v) => std::env::set_var("HDCD_TELEGRAM_BOT_TOKEN", v),
        None => std::env::remove_var("HDCD_TELEGRAM_BOT_TOKEN"),
    }
    match prev_tg {
        Some(v) => std::env::set_var("TELEGRAM_BOT_TOKEN", v),
        None => std::env::remove_var("TELEGRAM_BOT_TOKEN"),
    }
    match prev_state_dir {
        Some(v) => std::env::set_var("TELEGRAM_STATE_DIR", v),
        None => std::env::remove_var("TELEGRAM_STATE_DIR"),
    }
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_clean_env<F: FnOnce()>(f: F) {
        with_isolated_token_env(f);
    }

    #[test]
    fn env_var_hdcd_wins() {
        with_clean_env(|| {
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("HDCD_TELEGRAM_BOT_TOKEN", "from-hdcd");
            std::env::set_var("TELEGRAM_BOT_TOKEN", "from-legacy");
            assert_eq!(
                try_load_token(dir.path()).unwrap().as_deref(),
                Some("from-hdcd")
            );
        });
    }

    #[test]
    fn legacy_env_var_fallback() {
        with_clean_env(|| {
            let dir = tempfile::tempdir().unwrap();
            std::env::set_var("TELEGRAM_BOT_TOKEN", "legacy-only");
            assert_eq!(
                try_load_token(dir.path()).unwrap().as_deref(),
                Some("legacy-only")
            );
        });
    }

    #[test]
    fn env_file_hdcd_key() {
        with_clean_env(|| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join(".env"),
                "HDCD_TELEGRAM_BOT_TOKEN=from-file\n",
            )
            .unwrap();
            assert_eq!(
                try_load_token(dir.path()).unwrap().as_deref(),
                Some("from-file")
            );
        });
    }

    #[test]
    fn env_file_strips_quotes() {
        with_clean_env(|| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join(".env"),
                "HDCD_TELEGRAM_BOT_TOKEN=\"quoted-token\"\n",
            )
            .unwrap();
            assert_eq!(
                try_load_token(dir.path()).unwrap().as_deref(),
                Some("quoted-token")
            );
        });
    }

    #[test]
    fn nothing_configured_returns_none() {
        with_clean_env(|| {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(try_load_token(dir.path()).unwrap(), None);
        });
    }

    #[test]
    fn load_token_errors_when_absent() {
        with_clean_env(|| {
            let dir = tempfile::tempdir().unwrap();
            let err = load_token(dir.path()).unwrap_err().to_string();
            assert!(err.contains("Bot token required"));
        });
    }
}
