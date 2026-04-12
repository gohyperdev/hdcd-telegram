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
                    let _ = std::fs::rename(&tmp, &self.path);
                }
            }
            Err(e) => warn!(error = %e, "failed to serialize sessions.json"),
        }
    }

    /// Register a new session or update an existing one.
    pub fn register(&mut self, session_id: &str, topic_id: i64, reg: &Registration) {
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
            },
        );
        self.save();
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

    /// Check if a PID is still alive (platform-specific).
    pub fn is_pid_alive(pid: u32) -> bool {
        #[cfg(unix)]
        {
            // kill(pid, 0) checks existence without sending a signal.
            unsafe { libc::kill(pid as i32, 0) == 0 }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            use windows_sys::Win32::Foundation::CloseHandle;
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
        };

        reg.register("abc-123", 42, &registration);
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
        };
        reg.register("s1", 10, &registration);

        reg.close("s1");
        assert_eq!(
            reg.sessions["s1"].state,
            SessionState::Closed
        );
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
        };
        let r2 = Registration {
            session_id: "b".into(),
            label: "b".into(),
            pid: None,
            cwd: None,
            registered_at: "2026-04-11T15:00:00Z".into(),
            disconnected: false,
        };
        reg.register("a", 1, &r1);
        reg.register("b", 2, &r2);
        reg.close("a");

        let active = reg.active_sessions();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, "b");
    }
}
