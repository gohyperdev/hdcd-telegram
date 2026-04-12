// SPDX-License-Identifier: Apache-2.0

//! Filesystem-based IPC via JSONL mailbox files.
//!
//! Each session has two files:
//! - `inbox/<session-id>.jsonl`  — Telegram → session (router writes, MCP reads)
//! - `outbox/<session-id>.jsonl` — session → Telegram (MCP writes, router reads)
//!
//! Position tracking: a companion `.pos` file stores the byte offset of the
//! last-read position so that readers pick up only new lines after restart.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

// ---------------------------------------------------------------------------
// Inbox message (Telegram → Session)
// ---------------------------------------------------------------------------

/// A message written by the router into a session's inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub text: String,
    pub user: String,
    pub user_id: String,
    pub chat_id: String,
    pub message_id: i64,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_file_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_size: Option<String>,
}

// ---------------------------------------------------------------------------
// Outbox message (Session → Telegram)
// ---------------------------------------------------------------------------

/// A message written by the MCP server into its outbox for the router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxMessage {
    pub text: String,
    #[serde(default)]
    pub reply_to: Option<i64>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default = "default_format")]
    pub format: String,
    /// If set, edit this message instead of sending a new one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_message_id: Option<i64>,
    /// If set, add this emoji reaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub react_message_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub react_emoji: Option<String>,
    /// If set, rename the session's forum topic. `text` is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_to: Option<String>,
}

fn default_format() -> String {
    "text".into()
}

// ---------------------------------------------------------------------------
// Mailbox directories
// ---------------------------------------------------------------------------

/// Ensure inbox and outbox directories exist under the state dir.
pub fn ensure_dirs(state_dir: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let inbox = state_dir.join("inbox");
    let outbox = state_dir.join("outbox");
    let register = state_dir.join("register");
    std::fs::create_dir_all(&inbox).context("create inbox dir")?;
    std::fs::create_dir_all(&outbox).context("create outbox dir")?;
    std::fs::create_dir_all(&register).context("create register dir")?;
    Ok((inbox, outbox, register))
}

// ---------------------------------------------------------------------------
// JSONL writer
// ---------------------------------------------------------------------------

/// Append a single JSON line to a JSONL file.
///
/// Opens in append mode so multiple writers don't clobber each other
/// (for lines < 4096 bytes, append writes are atomic on most OS/FS combos).
pub fn append_line(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value).context("serialize JSONL line")?;
    line.push(b'\n');

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {} for append", path.display()))?;

    file.write_all(&line)
        .with_context(|| format!("write to {}", path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// JSONL reader with position tracking
// ---------------------------------------------------------------------------

/// Peek new lines since the last recorded position without advancing it.
/// Returns `(end_offset, message)` pairs where `end_offset` is the byte
/// offset right after that line (including its trailing `\n`).
///
/// Pair with [`commit_pos`] for at-least-once semantics: commit after a
/// message has been successfully processed so that a crash between read
/// and processing re-delivers unprocessed messages on restart.
///
/// Malformed lines are warned and skipped; their bytes are folded into
/// the next valid message's `end_offset`, so committing that offset also
/// advances past the bad lines. Partial trailing lines (no `\n` yet) are
/// left for the next read.
pub fn peek_new_lines<T: for<'de> Deserialize<'de>>(
    jsonl_path: &Path,
    pos_path: &Path,
) -> Result<Vec<(u64, T)>> {
    if !jsonl_path.exists() {
        return Ok(Vec::new());
    }

    let start = read_pos(pos_path);

    let mut file = std::fs::File::open(jsonl_path)
        .with_context(|| format!("open {}", jsonl_path.display()))?;

    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

    if start >= file_len {
        return Ok(Vec::new());
    }

    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("seek {} to {start}", jsonl_path.display()))?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("read {}", jsonl_path.display()))?;

    let mut offset = start;
    let mut messages = Vec::new();
    let mut parts = buf.split('\n').peekable();
    let mut line_idx = 0;
    while let Some(line) = parts.next() {
        // Last segment has no trailing `\n`. If it's non-empty the writer
        // is mid-append — leave it for the next tick.
        if parts.peek().is_none() {
            break;
        }
        let line_end = offset + line.len() as u64 + 1; // +1 for the `\n`
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            match serde_json::from_str::<T>(trimmed) {
                Ok(msg) => messages.push((line_end, msg)),
                Err(e) => warn!(
                    file = %jsonl_path.display(),
                    line_offset = line_idx,
                    error = %e,
                    "skipping malformed JSONL line"
                ),
            }
        }
        offset = line_end;
        line_idx += 1;
    }

    Ok(messages)
}

/// Advance the persisted read position. Use with [`peek_new_lines`] for
/// at-least-once semantics — commit after a message is successfully
/// processed so crashes don't silently drop unsent messages.
pub fn commit_pos(pos_path: &Path, pos: u64) {
    write_pos(pos_path, pos);
}

/// Read new lines and advance the position immediately (at-most-once,
/// fire-and-forget). Suitable for readers that don't need ACK semantics.
/// For at-least-once delivery use [`peek_new_lines`] + [`commit_pos`].
pub fn read_new_lines<T: for<'de> Deserialize<'de>>(
    jsonl_path: &Path,
    pos_path: &Path,
) -> Result<Vec<T>> {
    let peeked = peek_new_lines::<T>(jsonl_path, pos_path)?;
    if let Some((last_offset, _)) = peeked.last() {
        commit_pos(pos_path, *last_offset);
    }
    Ok(peeked.into_iter().map(|(_, m)| m).collect())
}

/// Path for the `.pos` companion file.
pub fn pos_path_for(jsonl_path: &Path) -> PathBuf {
    let mut p = jsonl_path.as_os_str().to_owned();
    p.push(".pos");
    PathBuf::from(p)
}

fn read_pos(pos_path: &Path) -> u64 {
    std::fs::read_to_string(pos_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_pos(pos_path: &Path, pos: u64) {
    let _ = std::fs::write(pos_path, pos.to_string());
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("test.jsonl");
        let pos = pos_path_for(&jsonl);

        // Write two messages.
        let m1 = InboxMessage {
            text: "hello".into(),
            user: "alice".into(),
            user_id: "1".into(),
            chat_id: "1".into(),
            message_id: 100,
            ts: "2026-04-11T15:00:00Z".into(),
            image_path: None,
            attachment_file_id: None,
            attachment_kind: None,
            attachment_name: None,
            attachment_mime: None,
            attachment_size: None,
        };
        let m2 = InboxMessage {
            text: "world".into(),
            user: "bob".into(),
            user_id: "2".into(),
            chat_id: "2".into(),
            message_id: 101,
            ts: "2026-04-11T15:01:00Z".into(),
            image_path: None,
            attachment_file_id: None,
            attachment_kind: None,
            attachment_name: None,
            attachment_mime: None,
            attachment_size: None,
        };
        append_line(&jsonl, &m1).unwrap();
        append_line(&jsonl, &m2).unwrap();

        // Read all — should get both.
        let msgs: Vec<InboxMessage> = read_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "hello");
        assert_eq!(msgs[1].text, "world");

        // Read again — nothing new.
        let msgs2: Vec<InboxMessage> = read_new_lines(&jsonl, &pos).unwrap();
        assert!(msgs2.is_empty());

        // Append a third, read only new.
        let m3 = InboxMessage {
            text: "third".into(),
            user: "charlie".into(),
            user_id: "3".into(),
            chat_id: "3".into(),
            message_id: 102,
            ts: "2026-04-11T15:02:00Z".into(),
            image_path: None,
            attachment_file_id: None,
            attachment_kind: None,
            attachment_name: None,
            attachment_mime: None,
            attachment_size: None,
        };
        append_line(&jsonl, &m3).unwrap();
        let msgs3: Vec<InboxMessage> = read_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(msgs3.len(), 1);
        assert_eq!(msgs3[0].text, "third");
    }

    #[test]
    fn read_nonexistent_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("missing.jsonl");
        let pos = pos_path_for(&jsonl);
        let msgs: Vec<InboxMessage> = read_new_lines(&jsonl, &pos).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn malformed_lines_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("bad.jsonl");
        let pos = pos_path_for(&jsonl);

        // Write valid, invalid, valid.
        let m1 = InboxMessage {
            text: "good1".into(),
            user: "a".into(),
            user_id: "1".into(),
            chat_id: "1".into(),
            message_id: 1,
            ts: "t".into(),
            image_path: None,
            attachment_file_id: None,
            attachment_kind: None,
            attachment_name: None,
            attachment_mime: None,
            attachment_size: None,
        };
        append_line(&jsonl, &m1).unwrap();

        // Append raw garbage.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .unwrap();
        f.write_all(b"NOT JSON\n").unwrap();

        let m2 = InboxMessage {
            text: "good2".into(),
            user: "b".into(),
            user_id: "2".into(),
            chat_id: "2".into(),
            message_id: 2,
            ts: "t".into(),
            image_path: None,
            attachment_file_id: None,
            attachment_kind: None,
            attachment_name: None,
            attachment_mime: None,
            attachment_size: None,
        };
        append_line(&jsonl, &m2).unwrap();

        let msgs: Vec<InboxMessage> = read_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "good1");
        assert_eq!(msgs[1].text, "good2");
    }

    #[test]
    fn outbox_message_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("outbox.jsonl");
        let pos = pos_path_for(&jsonl);

        let msg = OutboxMessage {
            text: "response".into(),
            reply_to: Some(456),
            files: vec!["/tmp/img.png".into()],
            format: "text".into(),
            edit_message_id: None,
            react_message_id: None,
            react_emoji: None,
            rename_to: None,
        };
        append_line(&jsonl, &msg).unwrap();

        let msgs: Vec<OutboxMessage> = read_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "response");
        assert_eq!(msgs[0].reply_to, Some(456));
        assert_eq!(msgs[0].files, vec!["/tmp/img.png"]);
    }

    fn make_outbox(text: &str) -> OutboxMessage {
        OutboxMessage {
            text: text.into(),
            reply_to: None,
            files: Vec::new(),
            format: "text".into(),
            edit_message_id: None,
            react_message_id: None,
            react_emoji: None,
            rename_to: None,
        }
    }

    #[test]
    fn peek_does_not_advance_pos() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("outbox.jsonl");
        let pos = pos_path_for(&jsonl);

        append_line(&jsonl, &make_outbox("one")).unwrap();
        append_line(&jsonl, &make_outbox("two")).unwrap();
        append_line(&jsonl, &make_outbox("three")).unwrap();

        // First peek returns all three.
        let peeked: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(peeked.len(), 3);
        assert!(peeked[0].0 < peeked[1].0);
        assert!(peeked[1].0 < peeked[2].0);

        // Peek again with no commit — still all three, proving pos
        // didn't advance.
        let peeked2: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(peeked2.len(), 3);
    }

    #[test]
    fn commit_per_message_enables_at_least_once() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("outbox.jsonl");
        let pos = pos_path_for(&jsonl);

        append_line(&jsonl, &make_outbox("one")).unwrap();
        append_line(&jsonl, &make_outbox("two")).unwrap();
        append_line(&jsonl, &make_outbox("three")).unwrap();

        // Process "one", commit. Simulate crash before "two".
        let peeked: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(peeked[0].1.text, "one");
        commit_pos(&pos, peeked[0].0);

        // Next tick after "crash" — "two" and "three" still visible.
        let after_crash: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(after_crash.len(), 2);
        assert_eq!(after_crash[0].1.text, "two");
        assert_eq!(after_crash[1].1.text, "three");
    }

    #[test]
    fn peek_skips_malformed_via_next_offset() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("outbox.jsonl");
        let pos = pos_path_for(&jsonl);

        append_line(&jsonl, &make_outbox("first")).unwrap();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .unwrap();
        f.write_all(b"NOT JSON\n").unwrap();
        append_line(&jsonl, &make_outbox("third")).unwrap();

        let peeked: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(peeked.len(), 2);
        assert_eq!(peeked[0].1.text, "first");
        assert_eq!(peeked[1].1.text, "third");

        // Committing the "third" offset also skips the bad line.
        commit_pos(&pos, peeked[1].0);
        let after: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert!(after.is_empty());
    }

    #[test]
    fn peek_leaves_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("outbox.jsonl");
        let pos = pos_path_for(&jsonl);

        append_line(&jsonl, &make_outbox("complete")).unwrap();
        // Simulate a writer mid-append: partial JSON, no trailing \n.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl)
            .unwrap();
        f.write_all(br#"{"text":"partial""#).unwrap();

        let peeked: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(peeked.len(), 1);
        assert_eq!(peeked[0].1.text, "complete");

        // Commit first message; now finish the partial line and peek again.
        commit_pos(&pos, peeked[0].0);
        f.write_all(b",\"files\":[],\"format\":\"text\"}\n").unwrap();

        let peeked2: Vec<(u64, OutboxMessage)> = peek_new_lines(&jsonl, &pos).unwrap();
        assert_eq!(peeked2.len(), 1);
        assert_eq!(peeked2[0].1.text, "partial");
    }
}
