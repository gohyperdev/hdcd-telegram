// SPDX-License-Identifier: Apache-2.0

//! Session registry — tracks active Claude Code sessions and their
//! associated forum topics. Persisted to `sessions.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// State of a session in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Closed,
}

/// A single session entry in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub topic_id: i64,
    pub label: String,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
    pub state: SessionState,
    pub registered_at: String,
    pub closed_at: Option<String>,
    /// Last rendered topic name. Lets the router skip redundant
    /// `editForumTopic` calls when the title hasn't changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Claude Code's sessionId (from `~/.claude/sessions/{PID}.json`).
    /// Stable across MCP restarts — enables `claude --resume <id>` to
    /// reattach to the original forum topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
    /// Count of real Telegram messages routed through this session —
    /// inbound (Telegram→MCP notifications) plus outbound (MCP `reply`
    /// and `edit_message` delivered). Lets us tell parasitic topics
    /// (never saw traffic) from topics worth preserving on rebind.
    #[serde(default)]
    pub message_count: u32,
}

/// Registration request written by an MCP server to `register/<session-id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub session_id: String,
    pub label: String,
    pub pid: Option<u32>,
    pub cwd: Option<String>,
    pub registered_at: String,
    #[serde(default)]
    pub disconnected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_session_id: Option<String>,
}

/// In-memory session registry backed by `sessions.json`.
#[derive(Debug)]
pub struct SessionRegistry {
    path: PathBuf,
    pub sessions: HashMap<String, SessionEntry>,
}

impl SessionRegistry {
    /// Load from disk or start empty.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join("sessions.json");
        let sessions = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(map) => map,
                Err(e) => {
                    warn!(error = %e, "sessions.json corrupt, starting fresh");
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
        Self { path, sessions }
    }

    /// Persist current state to disk.
    pub fn save(&self) {
        let tmp = format!("{}.tmp", self.path.display());
        match serde_json::to_string_pretty(&self.sessions) {
            Ok(json) => {
                if std::fs::write(&tmp, format!("{json}\n")).is_ok() {
                    let _ = crate::fs_perms::secure_file(std::path::Path::new(&tmp));
                    let _ = std::fs::rename(&tmp, &self.path);
                    let _ = crate::fs_perms::secure_file(&self.path);
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize sessions.json"),
        }
    }

    /// Bind a session to a topic. If another entry already owns the same
    /// topic (e.g. a closed prior session this `--resume`/`/resume` is
    /// reattaching to) it is evicted — one session_id ↔ one topic_id, no
    /// ambiguity. Any existing message_count for this topic carries over,
    /// so a reattached topic isn't treated as parasitic on later rebind.
    pub fn bind(&mut self, session_id: &str, topic_id: i64, reg: &Registration) {
        let mut carried: u32 = 0;
        let mut dupes: Vec<String> = Vec::new();
        for (id, e) in &self.sessions {
            if e.topic_id != topic_id {
                continue;
            }
            carried = carried.max(e.message_count);
            if id.as_str() != session_id {
                dupes.push(id.clone());
            }
        }
        for id in dupes {
            self.sessions.remove(&id);
        }

        self.sessions.insert(
            session_id.to_string(),
            SessionEntry {
                topic_id,
                label: reg.label.clone(),
                pid: reg.pid,
                cwd: reg.cwd.clone(),
                state: SessionState::Active,
                registered_at: reg.registered_at.clone(),
                closed_at: None,
                title: None,
                claude_session_id: reg.claude_session_id.clone(),
                message_count: carried,
            },
        );
        self.save();
    }

    /// Count of real Telegram messages observed on this session.
    pub fn activity_count(&self, session_id: &str) -> u32 {
        self.sessions
            .get(session_id)
            .map(|e| e.message_count)
            .unwrap_or(0)
    }

    /// Bump the activity counter. Called after a successful inbound
    /// notification or outbound delivery. Persists on every change so a
    /// router crash never loses activity that's already been sent.
    pub fn increment_activity(&mut self, session_id: &str) {
        if let Some(e) = self.sessions.get_mut(session_id) {
            e.message_count = e.message_count.saturating_add(1);
            self.save();
        }
    }

    /// Current claude_session_id for a session, if any.
    pub fn claude_session_id_of(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .get(session_id)
            .and_then(|e| e.claude_session_id.as_deref())
    }

    /// Find a prior entry matching the given Claude sessionId. Returns
    /// `(router_session_id, topic_id)` for any entry (active or closed) —
    /// used both to reattach on `claude --resume <id>` and to forget a
    /// stale entry whose Telegram topic has been deleted out from under us.
    pub fn find_by_claude_session(&self, claude_session_id: &str) -> Option<(&str, i64)> {
        self.sessions
            .iter()
            .find(|(_, e)| e.claude_session_id.as_deref() == Some(claude_session_id))
            .map(|(id, e)| (id.as_str(), e.topic_id))
    }

    /// Record a new topic title for a session. Returns `true` if the title
    /// actually changed (caller should then call `editForumTopic`).
    pub fn set_title(&mut self, session_id: &str, title: &str) -> bool {
        let entry = match self.sessions.get_mut(session_id) {
            Some(e) => e,
            None => return false,
        };
        if entry.title.as_deref() == Some(title) {
            return false;
        }
        entry.title = Some(title.to_string());
        self.save();
        true
    }

    /// Mark a session as closed.
    pub fn close(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.state = SessionState::Closed;
            entry.closed_at = Some(chrono::Utc::now().to_rfc3339());
            self.save();
        }
    }

    /// Drop a session entry entirely. Used when the bound topic has been
    /// deleted (parasitic with zero activity) — keeping the closed
    /// entry would leave a dangling topic_id that `find_by_claude_session`
    /// could later return and drive a failed reopen on resume.
    pub fn forget(&mut self, session_id: &str) {
        if self.sessions.remove(session_id).is_some() {
            self.save();
        }
    }

    /// Find session_id by topic_id.
    pub fn session_by_topic(&self, topic_id: i64) -> Option<&str> {
        self.sessions
            .iter()
            .find(|(_, e)| e.topic_id == topic_id && e.state == SessionState::Active)
            .map(|(id, _)| id.as_str())
    }

    /// Find topic_id by session_id.
    pub fn topic_by_session(&self, session_id: &str) -> Option<i64> {
        self.sessions.get(session_id).map(|e| e.topic_id)
    }

    /// List active sessions.
    pub fn active_sessions(&self) -> Vec<(&str, &SessionEntry)> {
        self.sessions
            .iter()
            .filter(|(_, e)| e.state == SessionState::Active)
            .map(|(id, e)| (id.as_str(), e))
            .collect()
    }

    /// IDs of active sessions whose recorded PID is no longer alive.
    /// Used by the startup reconcile + periodic health check to decide
    /// which topics to close. Sessions without a PID (pid = None) are
    /// treated as alive — we can't contradict what we don't know.
    pub fn dead_session_ids(&self) -> Vec<String> {
        self.active_sessions()
            .iter()
            .filter(|(_, e)| e.pid.map(|p| !Self::is_pid_alive(p)).unwrap_or(false))
            .map(|(id, _)| id.to_string())
            .collect()
    }

    /// Check if a PID is still alive (platform-specific).
    pub fn is_pid_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // kill(pid, 0) checks existence without sending a signal.
            unsafe { libc::kill(pid as i32, 0) == 0 }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            const STILL_ACTIVE: u32 = 259;
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if handle.is_null() {
                return false;
            }
            let mut exit_code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
            unsafe { CloseHandle(handle) };
            ok != 0 && exit_code == STILL_ACTIVE
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = pid;
            true // assume alive on unknown platforms
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());
        assert!(reg.sessions.is_empty());

        let registration = Registration {
            session_id: "abc-123".into(),
            label: "VS Code: test".into(),
            pid: Some(12345),
            cwd: Some("/home/user/project".into()),
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };

        reg.bind("abc-123", 42, &registration);
        assert_eq!(reg.sessions.len(), 1);
        assert_eq!(reg.topic_by_session("abc-123"), Some(42));
        assert_eq!(reg.session_by_topic(42), Some("abc-123"));

        // Reload from disk.
        let reg2 = SessionRegistry::load(dir.path());
        assert_eq!(reg2.sessions.len(), 1);
        assert_eq!(reg2.topic_by_session("abc-123"), Some(42));
    }

    #[test]
    fn close_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());

        let registration = Registration {
            session_id: "s1".into(),
            label: "test".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };
        reg.bind("s1", 10, &registration);

        reg.close("s1");
        assert_eq!(reg.sessions["s1"].state, SessionState::Closed);
        // Closed sessions not found by topic lookup.
        assert_eq!(reg.session_by_topic(10), None);
    }

    #[test]
    fn active_sessions_filter() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());

        let r1 = Registration {
            session_id: "a".into(),
            label: "a".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };
        let r2 = Registration {
            session_id: "b".into(),
            label: "b".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };
        reg.bind("a", 1, &r1);
        reg.bind("b", 2, &r2);
        reg.close("a");

        let active = reg.active_sessions();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, "b");
    }

    #[test]
    fn bind_evicts_prior_owner_and_carries_activity() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());

        let old = Registration {
            session_id: "old".into(),
            label: "old".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: Some("claude-X".into()),
        };
        reg.bind("old", 100, &old);
        reg.increment_activity("old");
        reg.increment_activity("old");
        reg.close("old");

        let new = Registration {
            session_id: "new".into(),
            label: "new".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:05:00Z".into(),
            disconnected: false,
            claude_session_id: Some("claude-X".into()),
        };
        reg.bind("new", 100, &new);

        assert!(!reg.sessions.contains_key("old"));
        assert_eq!(reg.activity_count("new"), 2);
        assert_eq!(reg.session_by_topic(100), Some("new"));
    }

    #[test]
    fn dead_session_ids_finds_sessions_with_nonexistent_pid() {
        // u32::MAX is reliably not a live PID on either OS. A session
        // whose bound PID is dead must show up in `dead_session_ids`.
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());

        let dead_reg = Registration {
            session_id: "dead".into(),
            label: "dead".into(),
            pid: Some(u32::MAX),
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };
        reg.bind("dead", 10, &dead_reg);

        let no_pid = Registration {
            session_id: "nopid".into(),
            label: "nopid".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };
        reg.bind("nopid", 11, &no_pid);

        let dead = reg.dead_session_ids();
        assert_eq!(dead, vec!["dead".to_string()]);
    }

    #[test]
    fn dead_session_ids_ignores_closed_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());
        let r = Registration {
            session_id: "s".into(),
            label: "s".into(),
            pid: Some(u32::MAX),
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
            claude_session_id: None,
        };
        reg.bind("s", 1, &r);
        reg.close("s");
        assert!(reg.dead_session_ids().is_empty());
    }

    #[test]
    fn increment_activity_saturates_on_unknown_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SessionRegistry::load(dir.path());
        reg.increment_activity("ghost"); // must not panic
        assert_eq!(reg.activity_count("ghost"), 0);
    }
}
