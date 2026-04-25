// SPDX-License-Identifier: Apache-2.0

//! `hdcd-router` — standalone process that holds the single Telegram
//! polling connection and routes messages between forum topics and
//! Claude Code sessions via filesystem IPC.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use hdcd_telegram::router::{config, mailbox, sessions, topics};
use hdcd_telegram::telegram::{api, polling, types};

/// Short timeout for the shutdown "Router stopped" announcement so a flaky
/// network can't wedge the process during exit.
const SHUTDOWN_SEND_TIMEOUT_SECS: u64 = 5;

/// Filename of the OS-level exclusive lock guard, kept next to `router.lock`.
/// The OS releases this lock on any process exit — normal, crash, kill, or
/// reboot — so there is no PID-reuse race or stale-heartbeat window.
const LOCK_GUARD_FILE: &str = "router.guard";

/// Shared state protected by a mutex for the update handler and outbox poller.
struct RouterState {
    registry: sessions::SessionRegistry,
    topic_mgr: topics::TopicManager,
    inbox_dir: PathBuf,
    outbox_dir: PathBuf,
    register_dir: PathBuf,
    config: config::RouterConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hdcd_router=info,hdcd_telegram=info".into()),
        )
        .init();

    let start_time = std::time::Instant::now();

    let sd = config::state_dir()?;

    // Refuse to start if another router is already alive. Two routers sharing
    // the same bot token both long-poll getUpdates and ping-pong 409 Conflict
    // forever, so neither delivers messages. The OS releases the lock on any
    // process termination (clean exit, crash, SIGKILL, reboot), so there is
    // no stale-lock window to wait out.
    let _lock_guard = match acquire_lock_guard(&sd) {
        Ok(g) => g,
        Err(e) => {
            error!(
                error = %e,
                "another hdcd-router is already running — refusing to start a second instance"
            );
            return Ok(());
        }
    };

    let cfg = config::load(&sd)
        .with_context(|| format!("failed to load config from {}/config.json", sd.display()))?;

    info!(
        supergroup = %cfg.supergroup_id,
        allowed_users = ?cfg.allowed_users,
        "router config loaded"
    );

    let (inbox_dir, outbox_dir, register_dir) = mailbox::ensure_dirs(&sd)?;

    let bot_api = Arc::new(api::BotApi::new(&cfg.bot_token));

    // Verify bot token.
    let me = bot_api
        .get_me()
        .await
        .context("getMe failed — check bot_token in config.json")?;
    let bot_username = me.username.unwrap_or_default();
    info!(username = %bot_username, "telegram bot identified");

    let registry = sessions::SessionRegistry::load(&sd);
    let topic_mgr = topics::TopicManager::new(Arc::clone(&bot_api), &cfg);

    let state = Arc::new(Mutex::new(RouterState {
        registry,
        topic_mgr,
        inbox_dir,
        outbox_dir: outbox_dir.clone(),
        register_dir: register_dir.clone(),
        config: cfg.clone(),
    }));

    // Reconcile stale sessions from a previous run.
    let stale = close_dead_sessions(&state).await;
    if stale.is_empty() {
        info!("no stale sessions to reconcile");
    } else {
        info!(
            count = stale.len(),
            "reconciled stale sessions from previous run"
        );
    }

    // Cancellation token for clean shutdown.
    let cancel = tokio_util::sync::CancellationToken::new();

    // Handle Ctrl+C.
    let shutdown_cancel = cancel.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                info!("received Ctrl+C, shutting down");
                shutdown_cancel.cancel();
            }
            Err(e) => error!(error = %e, "failed to listen for Ctrl+C"),
        }
    });

    // Start polling loop.
    let (update_tx, mut update_rx) = tokio::sync::mpsc::channel::<types::Update>(64);
    let poll_api = Arc::clone(&bot_api);
    let poll_cancel = cancel.clone();
    let poll_handle = tokio::spawn(async move {
        if let Err(e) = polling::run(poll_api, update_tx, poll_cancel).await {
            error!(error = %e, "polling loop exited with error");
        }
    });

    // Spawn registration watcher — polls register/ directory.
    let reg_state = Arc::clone(&state);
    let reg_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = reg_cancel.cancelled() => return,
            }
            if let Err(e) = process_registrations(&reg_state).await {
                warn!(error = %e, "registration scan failed");
            }
        }
    });

    // Spawn outbox poller — reads outbox files and sends to Telegram topics.
    let outbox_state = Arc::clone(&state);
    let outbox_api = Arc::clone(&bot_api);
    let outbox_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(
            cfg.outbox_poll_interval_ms,
        ));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = outbox_cancel.cancelled() => return,
            }
            poll_outbox(&outbox_state, &outbox_api).await;
        }
    });

    // Spawn health checker — detects dead PIDs and closes their topics.
    let health_state = Arc::clone(&state);
    let health_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_secs(cfg.health_check_interval_s));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = health_cancel.cancelled() => return,
            }
            close_dead_sessions(&health_state).await;
        }
    });

    // Spawn idle shutdown watcher — exits when no active sessions for grace period.
    let idle_state = Arc::clone(&state);
    let idle_cancel = cancel.clone();
    let auto_shutdown_delay = cfg.auto_shutdown_delay_s;
    if auto_shutdown_delay > 0 {
        tokio::spawn(async move {
            let mut idle_since: Option<tokio::time::Instant> = None;
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            // Skip the first immediate tick.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    _ = idle_cancel.cancelled() => return,
                }
                let s = idle_state.lock().await;
                let has_active = !s.registry.active_sessions().is_empty();
                drop(s);

                if has_active {
                    idle_since = None;
                } else {
                    let since = *idle_since.get_or_insert_with(tokio::time::Instant::now);
                    if since.elapsed().as_secs() >= auto_shutdown_delay {
                        info!(
                            idle_secs = since.elapsed().as_secs(),
                            "no active sessions, auto-shutting down"
                        );
                        idle_cancel.cancel();
                        return;
                    }
                }
            }
        });
    }

    // Spawn heartbeat writer — updates router.lock every 30s.
    let heartbeat_sd = sd.clone();
    let heartbeat_cancel = cancel.clone();
    write_heartbeat(&heartbeat_sd); // initial write
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = heartbeat_cancel.cancelled() => return,
            }
            write_heartbeat(&heartbeat_sd);
        }
    });

    info!("router started — listening for updates");

    // Announce startup in General topic.
    let startup_msg = format!(
        "\u{1f7e2} Router started \u{00b7} v{} \u{00b7} pid {} \u{00b7} built {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        exe_build_time(),
    );
    let _ = bot_api
        .send_message(&cfg.chat_id_str(), &startup_msg, None, None, None, None)
        .await;

    // Main loop: process incoming Telegram updates.
    let process_cancel = cancel.clone();
    tokio::select! {
        _ = async {
            while let Some(update) = update_rx.recv().await {
                if let Err(e) = handle_update(&update, &state).await {
                    warn!(error = %e, "failed to handle update");
                }
            }
        } => {}
        _ = process_cancel.cancelled() => {
            info!("update processor cancelled");
        }
    }

    // Shutdown — don't close active sessions. If their PIDs are still
    // alive they will be picked up on the next router start by reconcile.
    cancel.cancel();

    // Announce shutdown in General topic. Bounded so a wedged HTTP client
    // can't keep the process alive past shutdown.
    let shutdown_msg = format!(
        "\u{1f534} Router stopped \u{00b7} pid {} \u{00b7} uptime {}",
        std::process::id(),
        format_uptime(start_time.elapsed()),
    );
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(SHUTDOWN_SEND_TIMEOUT_SECS),
        bot_api.send_message(&cfg.chat_id_str(), &shutdown_msg, None, None, None, None),
    )
    .await;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), poll_handle).await;

    // Remove heartbeat file.
    let _ = std::fs::remove_file(sd.join("router.lock"));

    info!("router stopped");

    // Force exit: if any spawned task lingers (detached blocking I/O, HTTP
    // client background thread, etc.) the runtime drop can hang, leaving a
    // zombie process whose heartbeat task is already gone but whose lock
    // file gets stale. Exit 0 unconditionally after cleanup.
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// Inbound: Telegram → inbox
// ---------------------------------------------------------------------------

/// Handle an incoming Telegram update: route supergroup topic messages
/// to the correct session's inbox file.
async fn handle_update(update: &types::Update, state: &Arc<Mutex<RouterState>>) -> Result<()> {
    // ALLOWLIST INVARIANT: any new update variant handled below MUST call
    // is_allowed() before dispatching to session-affecting logic. The
    // early-return here only covers variants we ignore entirely.
    let msg = match &update.message {
        Some(m) => m,
        None => {
            let variant = if update.callback_query.is_some() {
                "callback_query"
            } else {
                "other"
            };
            debug!(
                update_id = update.update_id,
                variant, "skipping non-message update (allowlist enforced only on messages)"
            );
            return Ok(());
        }
    };

    let mut s = state.lock().await;
    let chat_id = msg.chat.id;

    // Only handle messages from the configured supergroup.
    if chat_id != s.config.supergroup_id {
        // Check allowed users for DMs (future: could be used for admin commands).
        debug!(chat_id, "ignoring message from non-supergroup chat");
        return Ok(());
    }

    // Check if sender is allowed.
    let sender_id = msg.from.as_ref().map(|u| u.id.to_string());
    let is_allowed = s.config.allowed_users.is_empty()
        || sender_id
            .as_ref()
            .map(|id| s.config.allowed_users.contains(id))
            .unwrap_or(false);

    if !is_allowed {
        debug!("message from non-allowed user, ignoring");
        return Ok(());
    }

    // Determine which topic the message belongs to.
    let thread_id = match msg.message_thread_id {
        Some(tid) => tid,
        None => {
            // Message in General topic — handle as router command.
            let api = Arc::clone(s.topic_mgr.api());
            let supergroup_id = s.config.chat_id_str();
            let text = msg.text.as_deref().unwrap_or("").to_string();

            // Collect active sessions as owned data before releasing lock.
            let active: Vec<(String, sessions::SessionEntry)> = s
                .registry
                .active_sessions()
                .into_iter()
                .map(|(id, e)| (id.to_string(), e.clone()))
                .collect();

            drop(s); // release lock before I/O

            // /kill needs mutable access to state — handle separately.
            if text.starts_with("/kill ") {
                let target = text.strip_prefix("/kill ").unwrap().trim();
                let reply = handle_kill_command(target, state).await;
                let _ = api
                    .send_message(&supergroup_id, &reply, None, None, None, None)
                    .await;
            } else if let Some(reply) = handle_general_command(&text, &active) {
                let _ = api
                    .send_message(&supergroup_id, &reply, None, None, None, None)
                    .await;
            }

            return Ok(());
        }
    };

    // Find the session for this topic.
    let session_id = match s.registry.session_by_topic(thread_id) {
        Some(id) => id.to_string(),
        None => {
            debug!(thread_id, "no session mapped to this topic");
            return Ok(());
        }
    };

    let from = msg.from.as_ref();
    let user = from.and_then(|u| u.username.as_deref()).unwrap_or("?");
    let user_id = from.map(|u| u.id.to_string()).unwrap_or_default();
    let text = msg
        .text
        .as_deref()
        .or(msg.caption.as_deref())
        .unwrap_or("")
        .to_string();
    let ts = chrono::DateTime::from_timestamp(msg.date, 0)
        .unwrap_or_default()
        .to_rfc3339();

    let inbox_msg = mailbox::InboxMessage {
        text,
        user: user.to_string(),
        user_id,
        chat_id: chat_id.to_string(),
        message_id: msg.message_id,
        ts,
        image_path: None, // TODO Phase 3+: download photos
        attachment_file_id: None,
        attachment_kind: None,
        attachment_name: None,
        attachment_mime: None,
        attachment_size: None,
    };

    let inbox_path = s.inbox_dir.join(format!("{session_id}.jsonl"));
    mailbox::append_line(&inbox_path, &inbox_msg)?;
    s.registry.increment_activity(&session_id);

    info!(
        session_id,
        thread_id,
        from = user,
        "routed message to inbox"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// General topic commands
// ---------------------------------------------------------------------------

/// Handle a text command sent in the General topic.
/// Returns `Some(reply)` for recognized commands, `None` otherwise.
fn handle_general_command(
    text: &str,
    active: &[(String, sessions::SessionEntry)],
) -> Option<String> {
    match text.trim() {
        "/status" => {
            if active.is_empty() {
                return Some("No active sessions.".to_string());
            }
            let mut lines = vec!["Active sessions:".to_string()];
            for (id, entry) in active {
                let pid = entry.pid.map(|p| format!(" (PID {p})")).unwrap_or_default();
                let cwd = entry.cwd.as_deref().unwrap_or("?");
                lines.push(format!(
                    "• {}{pid}\n  topic {} · {}",
                    entry.label, entry.topic_id, cwd
                ));
                lines.push(format!("  id: {id}"));
            }
            Some(lines.join("\n"))
        }
        "/help" => Some(
            "Router commands:\n\
             /status — list active sessions\n\
             /kill <session-id> — close a session\n\
             /help — show this help"
                .to_string(),
        ),
        _ => None,
    }
}

/// Handle `/kill <session-id>` — needs mutable state access.
async fn handle_kill_command(target: &str, state: &Arc<Mutex<RouterState>>) -> String {
    let mut s = state.lock().await;

    let has_topic = s.registry.topic_by_session(target).is_some();
    if !has_topic {
        return format!("Session not found: {target}");
    }

    let RouterState {
        topic_mgr,
        registry,
        ..
    } = &mut *s;
    topic_mgr.close_topic(target, registry).await;

    format!("Session killed: {target}")
}

// ---------------------------------------------------------------------------
// Outbound: outbox → Telegram
// ---------------------------------------------------------------------------

/// Poll all outbox files and send pending messages to their topics.
async fn poll_outbox(state: &Arc<Mutex<RouterState>>, api: &Arc<api::BotApi>) {
    let s = state.lock().await;

    // Collect active sessions to iterate.
    let active: Vec<(String, i64)> = s
        .registry
        .active_sessions()
        .iter()
        .map(|(id, e)| (id.to_string(), e.topic_id))
        .collect();

    let outbox_dir = s.outbox_dir.clone();
    let supergroup_id = s.config.chat_id_str();
    drop(s); // release lock before I/O

    for (session_id, topic_id) in active {
        let outbox_path = outbox_dir.join(format!("{session_id}.jsonl"));
        let pos_path = mailbox::pos_path_for(&outbox_path);

        // Peek — pos is committed per-message after successful delivery so
        // a crash between read and send doesn't silently drop messages.
        let messages: Vec<(u64, mailbox::OutboxMessage)> =
            match mailbox::peek_new_lines(&outbox_path, &pos_path) {
                Ok(m) => m,
                Err(e) => {
                    warn!(session_id, error = %e, "failed to read outbox");
                    continue;
                }
            };

        for (end_offset, msg) in messages {
            // Rename requests take a different path — they need mutable access
            // to the registry + TopicManager, not the raw Telegram API.
            if let Some(ref new_title) = msg.rename_to {
                let mut s = state.lock().await;
                let RouterState {
                    topic_mgr,
                    registry,
                    ..
                } = &mut *s;
                topic_mgr
                    .rename_topic(&session_id, new_title, registry)
                    .await;
                drop(s);
                mailbox::commit_pos(&pos_path, end_offset);
                continue;
            }

            match deliver_outbox_message(api, &supergroup_id, topic_id, &msg).await {
                Ok(()) => {
                    mailbox::commit_pos(&pos_path, end_offset);
                    let mut s = state.lock().await;
                    s.registry.increment_activity(&session_id);
                }
                Err(e) => {
                    warn!(
                        session_id,
                        error = %e,
                        "failed to deliver outbox message — will retry next tick"
                    );
                    // Don't advance pos; re-read this message on the next
                    // poll. Stop processing this session's batch to avoid
                    // reordering past the stuck message.
                    break;
                }
            }
        }
    }
}

/// Deliver a single outbox message to a Telegram forum topic.
async fn deliver_outbox_message(
    api: &api::BotApi,
    supergroup_id: &str,
    topic_id: i64,
    msg: &mailbox::OutboxMessage,
) -> Result<()> {
    // Handle reactions.
    if let (Some(react_msg_id), Some(ref emoji)) = (msg.react_message_id, &msg.react_emoji) {
        api.set_message_reaction(supergroup_id, react_msg_id, emoji)
            .await?;
        return Ok(());
    }

    let parse_mode = if msg.format == "markdownv2" {
        Some("MarkdownV2")
    } else {
        None
    };

    // Handle edits.
    if let Some(edit_id) = msg.edit_message_id {
        api.edit_message_text(supergroup_id, edit_id, &msg.text, parse_mode)
            .await?;
        return Ok(());
    }

    // Regular message send.
    api.send_message(
        supergroup_id,
        &msg.text,
        msg.reply_to,
        parse_mode,
        None,
        Some(topic_id),
    )
    .await?;

    // TODO: handle msg.files (send_photo / send_document)

    Ok(())
}

// ---------------------------------------------------------------------------
// Registration watcher
// ---------------------------------------------------------------------------

/// Scan the register/ directory for new or updated registration files.
async fn process_registrations(state: &Arc<Mutex<RouterState>>) -> Result<()> {
    let register_dir = {
        let s = state.lock().await;
        s.register_dir.clone()
    };

    let entries = match std::fs::read_dir(&register_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read registration");
                continue;
            }
        };

        let reg: sessions::Registration = match serde_json::from_str(&raw) {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "malformed registration");
                continue;
            }
        };

        if reg.disconnected {
            let mut s = state.lock().await;
            let has_topic = s.registry.topic_by_session(&reg.session_id).is_some();
            if has_topic {
                info!(session_id = %reg.session_id, "session disconnected");
                let RouterState {
                    topic_mgr,
                    registry,
                    ..
                } = &mut *s;
                topic_mgr.close_topic(&reg.session_id, registry).await;
            }
            drop(s);
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to remove disconnect marker"
                );
            }
            continue;
        }

        // Skip if already reconciled: session known AND claude_session_id
        // unchanged. Registration file stays on disk so subsequent rewrites
        // by the MCP-side sessionId watcher trigger rebind on next poll.
        {
            let s = state.lock().await;
            let known = s.registry.topic_by_session(&reg.session_id).is_some();
            let same_claude_id = s
                .registry
                .claude_session_id_of(&reg.session_id)
                .map(str::to_string)
                == reg.claude_session_id;
            if known && same_claude_id {
                continue;
            }
        }

        // Skip stale registrations whose MCP process is already dead.
        // Creating a topic just to have the health checker close it again
        // seconds later would spam the supergroup on router restart.
        if let Some(pid) = reg.pid {
            if !sessions::SessionRegistry::is_pid_alive(pid) {
                info!(
                    session_id = %reg.session_id,
                    pid,
                    "dropping stale registration — MCP PID is dead"
                );
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to remove stale registration"
                    );
                }
                continue;
            }
        }

        info!(
            session_id = %reg.session_id,
            label = %reg.label,
            pid = ?reg.pid,
            claude_session_id = reg.claude_session_id.as_deref().unwrap_or("?"),
            "reconciling session registration"
        );

        let mut s = state.lock().await;
        let RouterState {
            topic_mgr,
            registry,
            ..
        } = &mut *s;
        if let Err(e) = topic_mgr.reconcile_session(&reg, registry).await {
            error!(
                session_id = %reg.session_id,
                error = %e,
                "failed to reconcile session"
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Health checker
// ---------------------------------------------------------------------------

/// Collect active sessions whose MCP PID is no longer alive and close
/// their topics. Used both at startup (reconcile leftovers from the
/// previous run) and periodically by the health checker. Returns the
/// list of session_ids that were closed so the caller can log context.
async fn close_dead_sessions(state: &Arc<Mutex<RouterState>>) -> Vec<String> {
    let dead: Vec<String> = {
        let s = state.lock().await;
        s.registry.dead_session_ids()
    };

    for session_id in &dead {
        info!(session_id, "session PID dead, closing topic");
        let mut s = state.lock().await;
        let RouterState {
            topic_mgr,
            registry,
            ..
        } = &mut *s;
        topic_mgr.close_topic(session_id, registry).await;
    }
    dead
}

/// Format the build time as the mtime of the current executable, in local
/// time with a UTC offset so the reader isn't guessing which timezone.
fn exe_build_time() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%Y-%m-%d %H:%M %:z")
                .to_string()
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Human-friendly uptime: `3h 42m`, `42m`, or `17s`.
fn format_uptime(d: std::time::Duration) -> String {
    let s = d.as_secs();
    let h = s / 3600;
    let m = (s % 3600) / 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

/// Write a heartbeat file so MCP servers can detect a live router.
fn write_heartbeat(state_dir: &Path) {
    let lock_path = state_dir.join("router.lock");
    let pid = std::process::id();
    let ts = chrono::Utc::now().to_rfc3339();
    let content = format!("{{\"pid\":{pid},\"heartbeat\":\"{ts}\"}}\n");
    if std::fs::write(&lock_path, content).is_ok() {
        let _ = hdcd_telegram::fs_perms::secure_file(&lock_path);
    }
}

/// Try to acquire the OS-level exclusive lock on `router.guard`. The returned
/// `File` must be kept alive for the lifetime of the process — when it is
/// dropped (or the process exits for any reason, including SIGKILL or power
/// loss) the OS releases the lock and the next router can start immediately.
/// No PID-reuse race, no heartbeat staleness heuristic.
fn acquire_lock_guard(state_dir: &Path) -> Result<std::fs::File> {
    use fs4::fs_std::FileExt;

    let guard_path = state_dir.join(LOCK_GUARD_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&guard_path)
        .with_context(|| format!("open lock guard {}", guard_path.display()))?;
    let _ = hdcd_telegram::fs_perms::secure_file(&guard_path);

    let locked = file
        .try_lock_exclusive()
        .with_context(|| format!("lock {}", guard_path.display()))?;
    if !locked {
        anyhow::bail!("lock held by another process");
    }

    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_entry(label: &str, pid: Option<u32>, cwd: Option<&str>) -> sessions::SessionEntry {
        sessions::SessionEntry {
            topic_id: 42,
            label: label.to_string(),
            pid,
            cwd: cwd.map(|s| s.to_string()),
            state: sessions::SessionState::Active,
            registered_at: "2026-04-11T15:00:00Z".to_string(),
            closed_at: None,
            title: None,
            claude_session_id: None,
            message_count: 0,
        }
    }

    #[test]
    fn status_no_sessions() {
        let active: Vec<(String, sessions::SessionEntry)> = vec![];
        let reply = handle_general_command("/status", &active);
        assert_eq!(reply, Some("No active sessions.".to_string()));
    }

    #[test]
    fn status_with_sessions() {
        let active = vec![
            (
                "sess-1".to_string(),
                make_entry("VS Code: project", Some(1234), Some("/home/user/project")),
            ),
            ("sess-2".to_string(), make_entry("CLI: sweep", None, None)),
        ];
        let reply = handle_general_command("/status", &active).unwrap();
        assert!(reply.contains("VS Code: project"));
        assert!(reply.contains("PID 1234"));
        assert!(reply.contains("sess-1"));
        assert!(reply.contains("CLI: sweep"));
        assert!(reply.contains("sess-2"));
    }

    #[test]
    fn help_command() {
        let reply = handle_general_command("/help", &[]).unwrap();
        assert!(reply.contains("/status"));
        assert!(reply.contains("/kill"));
        assert!(reply.contains("/help"));
    }

    #[test]
    fn unknown_command_returns_none() {
        assert_eq!(handle_general_command("hello", &[]), None);
        assert_eq!(handle_general_command("/unknown", &[]), None);
    }

    #[test]
    fn heartbeat_writes_file() {
        let dir = tempfile::tempdir().unwrap();
        write_heartbeat(dir.path());
        let lock_path = dir.path().join("router.lock");
        assert!(lock_path.exists());
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(v["pid"].is_number());
        assert!(v["heartbeat"].is_string());
    }

    #[test]
    fn lock_guard_is_exclusive() {
        // First acquisition wins; second must fail immediately (not block)
        // so a double-start of the router surfaces as a clean error
        // instead of hanging on the OS lock.
        let dir = tempfile::tempdir().unwrap();
        let first = acquire_lock_guard(dir.path()).expect("first acquire");
        let err = acquire_lock_guard(dir.path()).expect_err("second should fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("lock") || msg.contains(LOCK_GUARD_FILE),
            "unexpected error: {msg}"
        );
        drop(first);

        // After the holder drops, a fresh acquire succeeds.
        let _third = acquire_lock_guard(dir.path()).expect("re-acquire after drop");
    }
}
