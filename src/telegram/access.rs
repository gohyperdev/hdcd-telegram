// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Access control — gate logic, `access.json` persistence, pairing.
//!
//! Format-compatible with the official Anthropic Telegram plugin so that
//! existing pairings survive the migration from Bun to Rust.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::warn;

// ---------------------------------------------------------------------------
// Data model (matches the TS plugin's access.json exactly)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    #[serde(default = "default_dm_policy")]
    pub dm_policy: DmPolicy,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub groups: HashMap<String, GroupPolicy>,
    #[serde(default)]
    pub pending: HashMap<String, PendingEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mention_patterns: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_reaction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to_mode: Option<ReplyToMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_chunk_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_mode: Option<ChunkMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    Disabled,
}

fn default_dm_policy() -> DmPolicy {
    DmPolicy::Pairing
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyToMode {
    Off,
    First,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChunkMode {
    Length,
    Newline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupPolicy {
    #[serde(default = "default_require_mention")]
    pub require_mention: bool,
    #[serde(default)]
    pub allow_from: Vec<String>,
}

fn default_require_mention() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEntry {
    pub sender_id: String,
    pub chat_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub replies: u32,
}

impl Default for Access {
    fn default() -> Self {
        Self {
            dm_policy: DmPolicy::Pairing,
            allow_from: Vec::new(),
            groups: HashMap::new(),
            pending: HashMap::new(),
            mention_patterns: None,
            ack_reaction: None,
            reply_to_mode: None,
            text_chunk_limit: None,
            chunk_mode: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Gate result
// ---------------------------------------------------------------------------

/// Outcome of the inbound message gate.
#[derive(Debug, Clone)]
pub enum GateResult {
    /// Message should be delivered to Claude.
    Deliver { access: Access },
    /// Message is silently dropped.
    Drop,
    /// Sender needs to pair. `code` is the 6-hex-char pairing code.
    Pair { code: String, is_resend: bool },
}

// ---------------------------------------------------------------------------
// File I/O
// ---------------------------------------------------------------------------

/// Read access.json. Returns default if file is missing. Renames corrupt
/// files aside so we never loop on parse failures.
pub fn load_access(state_dir: &Path) -> Access {
    let path = access_path(state_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Access>(&raw) {
            Ok(a) => a,
            Err(e) => {
                warn!(error = %e, "access.json is corrupt, moving aside");
                let backup = format!("{}.corrupt-{}", path.display(), epoch_ms());
                let _ = std::fs::rename(&path, &backup);
                Access::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Access::default(),
        Err(e) => {
            warn!(error = %e, "failed to read access.json");
            Access::default()
        }
    }
}

pub fn save_access(state_dir: &Path, access: &Access) {
    let path = access_path(state_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = format!("{}.tmp", path.display());
    match serde_json::to_string_pretty(access) {
        Ok(json) => {
            if std::fs::write(&tmp, format!("{json}\n")).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize access.json"),
    }
}

fn access_path(state_dir: &Path) -> PathBuf {
    state_dir.join("access.json")
}

// ---------------------------------------------------------------------------
// Gate logic
// ---------------------------------------------------------------------------

/// Evaluate whether an inbound message should be delivered, dropped, or
/// trigger a pairing flow. Mirrors the TS plugin's `gate()` function.
pub fn gate(
    state_dir: &Path,
    sender_id: &str,
    chat_id: &str,
    chat_type: &str,
    is_mentioned: bool,
) -> GateResult {
    let mut access = load_access(state_dir);
    let pruned = prune_expired(&mut access);
    if pruned {
        save_access(state_dir, &access);
    }

    if access.dm_policy == DmPolicy::Disabled {
        return GateResult::Drop;
    }

    match chat_type {
        "private" => gate_private(state_dir, &mut access, sender_id, chat_id),
        "group" | "supergroup" => {
            gate_group(&access, sender_id, chat_id, is_mentioned)
        }
        _ => GateResult::Drop,
    }
}

fn gate_private(
    state_dir: &Path,
    access: &mut Access,
    sender_id: &str,
    chat_id: &str,
) -> GateResult {
    if access.allow_from.contains(&sender_id.to_string()) {
        return GateResult::Deliver {
            access: access.clone(),
        };
    }
    if access.dm_policy == DmPolicy::Allowlist {
        return GateResult::Drop;
    }

    // Pairing mode — check for existing pending code.
    for (code, p) in access.pending.iter_mut() {
        if p.sender_id == sender_id {
            if p.replies >= 2 {
                return GateResult::Drop;
            }
            p.replies += 1;
            let code = code.clone();
            save_access(state_dir, access);
            return GateResult::Pair {
                code,
                is_resend: true,
            };
        }
    }

    // Cap pending at 3.
    if access.pending.len() >= 3 {
        return GateResult::Drop;
    }

    let code = generate_pairing_code();
    let now = epoch_ms();
    access.pending.insert(
        code.clone(),
        PendingEntry {
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            created_at: now,
            expires_at: now + 60 * 60 * 1000, // 1 hour
            replies: 1,
        },
    );
    save_access(state_dir, access);
    GateResult::Pair {
        code,
        is_resend: false,
    }
}

fn gate_group(
    access: &Access,
    sender_id: &str,
    chat_id: &str,
    is_mentioned: bool,
) -> GateResult {
    let policy = match access.groups.get(chat_id) {
        Some(p) => p,
        None => return GateResult::Drop,
    };

    if !policy.allow_from.is_empty() && !policy.allow_from.contains(&sender_id.to_string()) {
        return GateResult::Drop;
    }

    if policy.require_mention && !is_mentioned {
        return GateResult::Drop;
    }

    GateResult::Deliver {
        access: access.clone(),
    }
}

/// Check that an outbound chat_id is allowed (for reply/react/edit tools).
///
/// `allow_from` serves double duty: it holds the user IDs of paired DM users.
/// In Telegram DMs, user_id == chat_id, so checking `allow_from` also covers
/// DM chat IDs. Group chat IDs are always negative, so there is no collision
/// risk between DM user IDs and group chat IDs.
pub fn assert_allowed_chat(state_dir: &Path, chat_id: &str) -> Result<()> {
    let access = load_access(state_dir);
    if access.allow_from.contains(&chat_id.to_string()) {
        return Ok(());
    }
    if access.groups.contains_key(chat_id) {
        return Ok(());
    }
    anyhow::bail!("chat {chat_id} is not allowlisted -- add via /telegram:access")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn prune_expired(access: &mut Access) -> bool {
    let now = epoch_ms();
    let before = access.pending.len();
    access.pending.retain(|_, p| p.expires_at >= now);
    access.pending.len() != before
}

fn generate_pairing_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 3] = rng.gen();
    hex::encode(bytes)
}

fn epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Find the largest valid char boundary at or before `index` in `s`.
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    let mut i = index;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Chunk text for Telegram's 4096-char limit.
pub fn chunk_text(text: &str, limit: usize, mode: ChunkMode) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > limit {
        // Ensure we never slice in the middle of a multi-byte char.
        let safe_limit = floor_char_boundary(rest, limit);
        let cut = match mode {
            ChunkMode::Newline => {
                let para = rest[..safe_limit].rfind("\n\n");
                let line = rest[..safe_limit].rfind('\n');
                let space = rest[..safe_limit].rfind(' ');
                let half = safe_limit / 2;
                if para.map(|i| i > half).unwrap_or(false) {
                    para.unwrap()
                } else if line.map(|i| i > half).unwrap_or(false) {
                    line.unwrap()
                } else if let Some(s) = space {
                    if s > 0 {
                        s
                    } else {
                        safe_limit
                    }
                } else {
                    safe_limit
                }
            }
            ChunkMode::Length => safe_limit,
        };
        out.push(rest[..cut].to_string());
        rest = &rest[cut..];
        rest = rest.trim_start_matches('\n');
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// Safe-name sanitizer for filenames from Telegram.
/// Only allows alphanumeric, dot, hyphen, underscore, and space.
pub fn safe_name(s: &str) -> String {
    // First strip any path-traversal sequences.
    let stripped = s.replace("..", "").replace(['/', '\\'], "");
    stripped
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' '))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_access_roundtrips_through_json() {
        let a = Access::default();
        let json = serde_json::to_string_pretty(&a).unwrap();
        let back: Access = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dm_policy, DmPolicy::Pairing);
        assert!(back.allow_from.is_empty());
        assert!(back.groups.is_empty());
    }

    #[test]
    fn gate_delivers_when_sender_is_allowed() {
        let dir = tempdir().unwrap();
        let access = Access {
            allow_from: vec!["123".into()],
            ..Access::default()
        };
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "123", "123", "private", false);
        assert!(matches!(result, GateResult::Deliver { .. }));
    }

    #[test]
    fn gate_drops_unknown_sender_in_allowlist_mode() {
        let dir = tempdir().unwrap();
        let access = Access {
            dm_policy: DmPolicy::Allowlist,
            allow_from: vec!["123".into()],
            ..Access::default()
        };
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "456", "456", "private", false);
        assert!(matches!(result, GateResult::Drop));
    }

    #[test]
    fn gate_issues_pairing_code_for_unknown_sender() {
        let dir = tempdir().unwrap();
        let access = Access::default(); // pairing mode
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "789", "789", "private", false);
        match result {
            GateResult::Pair { code, is_resend } => {
                assert!(!is_resend);
                assert_eq!(code.len(), 6);
            }
            other => panic!("expected Pair, got {other:?}"),
        }
    }

    #[test]
    fn gate_resends_existing_pairing_code() {
        let dir = tempdir().unwrap();
        let access = Access::default();
        save_access(dir.path(), &access);

        // First attempt generates a code.
        let result = gate(dir.path(), "789", "789", "private", false);
        let code = match result {
            GateResult::Pair { code, .. } => code,
            other => panic!("expected Pair, got {other:?}"),
        };

        // Second attempt resends the same code.
        let result2 = gate(dir.path(), "789", "789", "private", false);
        match result2 {
            GateResult::Pair {
                code: c2,
                is_resend,
            } => {
                assert!(is_resend);
                assert_eq!(c2, code);
            }
            other => panic!("expected Pair resend, got {other:?}"),
        }
    }

    #[test]
    fn gate_drops_after_max_replies() {
        let dir = tempdir().unwrap();
        let access = Access::default();
        save_access(dir.path(), &access);

        // Attempt 1: pair
        let _ = gate(dir.path(), "789", "789", "private", false);
        // Attempt 2: resend (replies=2)
        let _ = gate(dir.path(), "789", "789", "private", false);
        // Attempt 3: should drop
        let result = gate(dir.path(), "789", "789", "private", false);
        assert!(matches!(result, GateResult::Drop));
    }

    #[test]
    fn gate_drops_disabled() {
        let dir = tempdir().unwrap();
        let access = Access {
            dm_policy: DmPolicy::Disabled,
            ..Access::default()
        };
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "123", "123", "private", false);
        assert!(matches!(result, GateResult::Drop));
    }

    #[test]
    fn gate_group_delivers_when_configured() {
        let dir = tempdir().unwrap();
        let mut groups = HashMap::new();
        groups.insert(
            "-100123".to_string(),
            GroupPolicy {
                require_mention: false,
                allow_from: vec![],
            },
        );
        let access = Access {
            groups,
            ..Access::default()
        };
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "456", "-100123", "supergroup", false);
        assert!(matches!(result, GateResult::Deliver { .. }));
    }

    #[test]
    fn gate_group_drops_when_not_mentioned() {
        let dir = tempdir().unwrap();
        let mut groups = HashMap::new();
        groups.insert(
            "-100123".to_string(),
            GroupPolicy {
                require_mention: true,
                allow_from: vec![],
            },
        );
        let access = Access {
            groups,
            ..Access::default()
        };
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "456", "-100123", "supergroup", false);
        assert!(matches!(result, GateResult::Drop));

        let result2 = gate(dir.path(), "456", "-100123", "supergroup", true);
        assert!(matches!(result2, GateResult::Deliver { .. }));
    }

    #[test]
    fn gate_group_restricts_allowfrom() {
        let dir = tempdir().unwrap();
        let mut groups = HashMap::new();
        groups.insert(
            "-100123".to_string(),
            GroupPolicy {
                require_mention: false,
                allow_from: vec!["111".into()],
            },
        );
        let access = Access {
            groups,
            ..Access::default()
        };
        save_access(dir.path(), &access);

        let result = gate(dir.path(), "222", "-100123", "supergroup", false);
        assert!(matches!(result, GateResult::Drop));

        let result2 = gate(dir.path(), "111", "-100123", "supergroup", false);
        assert!(matches!(result2, GateResult::Deliver { .. }));
    }

    #[test]
    fn pending_caps_at_three() {
        let dir = tempdir().unwrap();
        let access = Access::default();
        save_access(dir.path(), &access);

        // Three pairings fill the cap.
        for i in 1..=3 {
            let id = format!("{i}00");
            let r = gate(dir.path(), &id, &id, "private", false);
            assert!(matches!(r, GateResult::Pair { .. }));
        }
        // Fourth is dropped.
        let r = gate(dir.path(), "400", "400", "private", false);
        assert!(matches!(r, GateResult::Drop));
    }

    #[test]
    fn chunk_text_identity_for_short() {
        let result = chunk_text("hello", 4096, ChunkMode::Length);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn chunk_text_splits_long() {
        let text = "a".repeat(5000);
        let result = chunk_text(&text, 4096, ChunkMode::Length);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 4096);
    }

    #[test]
    fn chunk_text_newline_prefers_paragraph() {
        // Place the paragraph break past the halfway point so the chunker picks it.
        let text = format!("{}.\n\n{}", "a".repeat(3000), "b".repeat(2000));
        let result = chunk_text(&text, 4096, ChunkMode::Newline);
        assert_eq!(result.len(), 2);
        assert!(result[0].ends_with('.'));
    }

    #[test]
    fn parses_ts_plugin_access_json() {
        // Verify we can parse a file written by the TS plugin.
        let json = "{
            \"dmPolicy\": \"pairing\",
            \"allowFrom\": [\"123456789\"],
            \"groups\": { \"-100123\": { \"requireMention\": true, \"allowFrom\": [] } },
            \"pending\": { \"a1b2c3\": { \"senderId\": \"111\", \"chatId\": \"111\", \"createdAt\": 1000, \"expiresAt\": 9999999999999, \"replies\": 1 } },
            \"ackReaction\": \"\u{1f44d}\",
            \"replyToMode\": \"first\",
            \"textChunkLimit\": 4096,
            \"chunkMode\": \"length\"
        }";
        let a: Access = serde_json::from_str(json).unwrap();
        assert_eq!(a.dm_policy, DmPolicy::Pairing);
        assert_eq!(a.allow_from, vec!["123456789"]);
        assert!(a.groups.contains_key("-100123"));
        assert!(a.pending.contains_key("a1b2c3"));
        assert_eq!(a.pending["a1b2c3"].sender_id, "111");
    }
}
