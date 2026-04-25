// SPDX-License-Identifier: Apache-2.0

//! Discovery of Claude Code's `sessionId` from the MCP process.
//!
//! Claude Code writes `~/.claude/sessions/{PID}.json` for every running
//! session (CLI, VS Code, or desktop). The file contains the stable
//! `sessionId` and the session's `cwd`. We walk up from our own PPID to
//! find the parent process that is Claude Code, then read its file.
//!
//! The sessionId can change during an MCP's lifetime — notably:
//! - `claude --resume` without an ID shows an interactive picker; the
//!   file is first written with a fresh UUID, then rewritten with the
//!   picked sessionId after the user chooses (arbitrary wait — user may
//!   take seconds or minutes).
//! - `/resume <id>` inside a session switches to another transcript.
//! - `/clear` starts a fresh sessionId in the same process.
//!
//! Because the wait is unbounded, we do **not** block at startup. MCP
//! reads the current value once and proceeds; a background watcher
//! observes the file for changes and lets the router rebind topics
//! after the fact.

use std::path::PathBuf;

use serde::Deserialize;

/// Env var that disables all sessionId discovery (read and watch). Set by
/// the integration test which runs outside a real Claude process.
pub const SKIP_ENV: &str = "HDCD_SKIP_SESSION_DISCOVERY";

/// Max depth to walk when looking for Claude in the parent chain.
/// Wrappers (cmd.exe, devcontainer launchers, shell aliases) can insert
/// a layer or two. 8 is ample — real chains are 1–3 deep.
const MAX_PARENT_WALK: u32 = 8;

/// Deserialized `~/.claude/sessions/{PID}.json`. Extra fields tolerated.
#[derive(Debug, Clone, Deserialize)]
struct PidInfo {
    #[serde(rename = "sessionId")]
    session_id: String,
}

fn pid_info_path(pid: u32) -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sessions")
            .join(format!("{pid}.json")),
    )
}

fn read_session_id(pid: u32) -> Option<String> {
    let path = pid_info_path(pid)?;
    let raw = std::fs::read_to_string(path).ok()?;
    let info: PidInfo = serde_json::from_str(&raw).ok()?;
    Some(info.session_id)
}

fn parent_pid() -> Option<u32> {
    #[cfg(unix)]
    unsafe {
        Some(libc::getppid() as u32)
    }
    #[cfg(windows)]
    {
        parent_of(std::process::id())
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 2..)?;
    let mut it = rest.split_whitespace();
    let _state = it.next()?;
    it.next()?.parse().ok()
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }

    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let ppid = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (ppid > 0).then_some(ppid)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

#[cfg(windows)]
fn parent_of(pid: u32) -> Option<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut found = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    found = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

#[cfg(not(any(unix, windows)))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

/// Walk the parent chain starting from our PPID, returning the first PID
/// that has a readable `sessions/{pid}.json` file. `None` if no ancestor
/// within `MAX_PARENT_WALK` is a Claude process.
pub fn find_claude_pid() -> Option<u32> {
    let mut pid = parent_pid()?;
    for _ in 0..MAX_PARENT_WALK {
        if read_session_id(pid).is_some() {
            return Some(pid);
        }
        pid = match parent_of(pid) {
            Some(p) if p != 0 && p != pid => p,
            _ => break,
        };
    }
    None
}

/// Read the current sessionId from Claude's PID file, synchronously. No
/// retries, no waiting — designed to be called often from a watcher poll
/// loop. Returns `None` if the file is missing, unreadable, or discovery
/// is disabled via `HDCD_SKIP_SESSION_DISCOVERY`.
pub fn current_session_id(claude_pid: u32) -> Option<String> {
    if std::env::var_os(SKIP_ENV).is_some() {
        return None;
    }
    read_session_id(claude_pid)
}

/// Discover Claude's PID (parent-chain walk) unless discovery is disabled.
pub fn discover_claude_pid() -> Option<u32> {
    if std::env::var_os(SKIP_ENV).is_some() {
        return None;
    }
    find_claude_pid()
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::parent_of;

    #[test]
    fn parent_of_self_returns_some() {
        assert!(parent_of(std::process::id()).is_some());
    }
}
