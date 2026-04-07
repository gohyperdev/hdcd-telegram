// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Inbound message handlers — text, photo, document, voice, etc.
//!
//! Each handler evaluates the gate, emits pairing replies for unknown senders,
//! and formats approved messages as `notifications/claude/channel` MCP frames.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use super::access::{self, Access, ChunkMode, GateResult, ReplyToMode};
use super::api::BotApi;
use super::transcribe::{TranscribeConfig, TranscribeSupport};
use super::types::{Message, Update};

/// Attachment metadata extracted from an inbound message.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub kind: String,
    pub file_id: String,
    pub size: Option<i64>,
    pub mime: Option<String>,
    pub name: Option<String>,
}

/// A voice transcription awaiting user confirmation.
#[derive(Debug, Clone)]
pub struct PendingTranscription {
    pub transcription: String,
    pub chat_id: String,
    pub original_message_id: String,
    pub user_id: String,
    pub created_at: Instant,
    pub attachment_meta: Option<AttachmentMeta>,
}

/// Pending transcription timeout (5 minutes).
const PENDING_TRANSCRIPTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Shared context for all handlers.
pub struct HandlerContext {
    pub api: Arc<BotApi>,
    pub state_dir: PathBuf,
    pub inbox_dir: PathBuf,
    pub bot_username: String,
    pub transcribe_support: TranscribeSupport,
    pub transcribe_config: TranscribeConfig,
    pub pending_transcriptions: Mutex<HashMap<(String, String), PendingTranscription>>,
}

/// Permission-reply regex: `yes <5-char-id>` or `no <5-char-id>`.
/// The 5-char id uses lowercase a-z minus 'l'.
fn permission_reply_match(text: &str) -> Option<(bool, String)> {
    // Matches: y/yes/n/no followed by exactly 5 lowercase letters (a-k, m-z).
    let trimmed = text.trim();
    static RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"(?i)^\s*(y|yes|n|no)\s+([a-km-z]{5})\s*$").unwrap()
    });
    let re = &*RE;
    let caps = re.captures(trimmed)?;
    let verdict = caps[1].to_lowercase();
    let allow = verdict.starts_with('y');
    let request_id = caps[2].to_lowercase();
    Some((allow, request_id))
}

/// Process a single Telegram update. Returns a JSON-RPC notification frame
/// to write to stdout, or `None` if the message should be dropped or handled
/// internally (pairing, permission relay, commands).
pub async fn process_update(update: &Update, ctx: &HandlerContext) -> Option<Vec<Value>> {
    // Handle callback queries (permission inline buttons).
    if let Some(cb) = &update.callback_query {
        return handle_callback_query(cb, ctx).await;
    }

    let msg = update.message.as_ref()?;
    let from = msg.from.as_ref()?;
    let sender_id = from.id.to_string();
    let chat_id = msg.chat.id.to_string();

    // --- Transcription confirmation intercept (before gate) ---
    if let Some(text) = &msg.text {
        if let Some(frames) =
            check_transcription_confirmation(text, &chat_id, &sender_id, ctx).await
        {
            return Some(frames);
        }
    }

    // Bot commands — DM only.
    if msg.chat.chat_type == "private" {
        if let Some(text) = &msg.text {
            if text.starts_with('/') {
                handle_command(msg, text, ctx).await;
                return None;
            }
        }
    }

    let is_mentioned = check_mention(msg, &ctx.bot_username);
    let result = access::gate(
        &ctx.state_dir,
        &sender_id,
        &chat_id,
        &msg.chat.chat_type,
        is_mentioned,
    );

    match result {
        GateResult::Drop => None,
        GateResult::Pair { code, is_resend } => {
            let lead = if is_resend {
                "Still pending"
            } else {
                "Pairing required"
            };
            let text =
                format!("{lead} \u{2014} run in Claude Code:\n\n/telegram:access pair {code}");
            if let Err(e) = ctx
                .api
                .send_message(&chat_id, &text, None, None, None)
                .await
            {
                warn!(error = %e, "failed to send pairing reply");
            }
            None
        }
        GateResult::Deliver { access } => handle_deliver(msg, &access, ctx).await,
    }
}

/// Handle an approved inbound message: extract content + meta, emit
/// notification frames (possibly including permission-relay events).
async fn handle_deliver(
    msg: &Message,
    access: &Access,
    ctx: &HandlerContext,
) -> Option<Vec<Value>> {
    let from = msg.from.as_ref()?;
    let chat_id = msg.chat.id.to_string();
    let msg_id = msg.message_id;

    // Extract text and optional attachment.
    let (mut text, attachment, download_photo) = extract_content(msg);

    // Voice message transcription.
    if msg.voice.is_some() {
        if let Some(frames) =
            handle_voice_transcription(msg, &mut text, &attachment, &chat_id, ctx).await
        {
            // Echo-transcript mode: we sent the transcript back for confirmation.
            // The actual notification will be emitted when the user confirms.
            return Some(frames);
        }
        // If handle_voice_transcription returned None, `text` may have been
        // updated in-place (transcription appended, or unavailability note).
    }

    // Permission-reply intercept.
    if let Some((allow, request_id)) = permission_reply_match(&text) {
        let behavior = if allow { "allow" } else { "deny" };
        let frame = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel/permission",
            "params": {
                "request_id": request_id,
                "behavior": behavior,
            }
        });
        // React with checkmark / cross.
        let emoji = if allow { "\u{2705}" } else { "\u{274c}" };
        let _ = ctx.api.set_message_reaction(&chat_id, msg_id, emoji).await;
        return Some(vec![frame]);
    }

    // Typing indicator.
    let _ = ctx.api.send_chat_action(&chat_id, "typing").await;

    // Ack reaction.
    if let Some(ref ack) = access.ack_reaction {
        if !ack.is_empty() {
            let _ = ctx.api.set_message_reaction(&chat_id, msg_id, ack).await;
        }
    }

    // Download photo if present (only after gate approved).
    let image_path = if download_photo {
        download_photo_from_message(msg, ctx).await
    } else {
        None
    };

    // Build meta map.
    let mut meta = serde_json::Map::new();
    meta.insert("chat_id".into(), json!(chat_id));
    meta.insert("message_id".into(), json!(msg_id.to_string()));
    meta.insert(
        "user".into(),
        json!(from.username.as_deref().unwrap_or(&from.id.to_string())),
    );
    meta.insert("user_id".into(), json!(from.id.to_string()));
    meta.insert(
        "ts".into(),
        json!(chrono::DateTime::from_timestamp(msg.date, 0)
            .unwrap_or_default()
            .to_rfc3339()),
    );
    if let Some(ref path) = image_path {
        meta.insert("image_path".into(), json!(path));
    }
    if let Some(ref att) = attachment {
        meta.insert("attachment_kind".into(), json!(att.kind));
        meta.insert("attachment_file_id".into(), json!(att.file_id));
        if let Some(size) = att.size {
            meta.insert("attachment_size".into(), json!(size.to_string()));
        }
        if let Some(ref mime) = att.mime {
            meta.insert("attachment_mime".into(), json!(mime));
        }
        if let Some(ref name) = att.name {
            meta.insert("attachment_name".into(), json!(access::safe_name(name)));
        }
    }

    let frame = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": text,
            "meta": Value::Object(meta),
        }
    });

    Some(vec![frame])
}

/// Extract text content and optional attachment from a message.
/// Returns (text, attachment_meta, should_download_photo).
fn extract_content(msg: &Message) -> (String, Option<AttachmentMeta>, bool) {
    if let Some(photos) = &msg.photo {
        if !photos.is_empty() {
            let text = msg.caption.clone().unwrap_or_else(|| "(photo)".into());
            return (text, None, true);
        }
    }
    if let Some(doc) = &msg.document {
        let name = doc.file_name.as_deref().map(access::safe_name);
        let text = msg
            .caption
            .clone()
            .unwrap_or_else(|| format!("(document: {})", name.as_deref().unwrap_or("file")));
        return (
            text,
            Some(AttachmentMeta {
                kind: "document".into(),
                file_id: doc.file_id.clone(),
                size: doc.file_size,
                mime: doc.mime_type.clone(),
                name,
            }),
            false,
        );
    }
    if let Some(voice) = &msg.voice {
        let text = msg
            .caption
            .clone()
            .unwrap_or_else(|| "(voice message)".into());
        return (
            text,
            Some(AttachmentMeta {
                kind: "voice".into(),
                file_id: voice.file_id.clone(),
                size: voice.file_size,
                mime: voice.mime_type.clone(),
                name: None,
            }),
            false,
        );
    }
    if let Some(audio) = &msg.audio {
        let name = audio.file_name.as_deref().map(access::safe_name);
        let display = audio
            .title
            .as_deref()
            .map(access::safe_name)
            .or_else(|| name.clone())
            .unwrap_or_else(|| "audio".into());
        let text = msg
            .caption
            .clone()
            .unwrap_or_else(|| format!("(audio: {display})"));
        return (
            text,
            Some(AttachmentMeta {
                kind: "audio".into(),
                file_id: audio.file_id.clone(),
                size: audio.file_size,
                mime: audio.mime_type.clone(),
                name,
            }),
            false,
        );
    }
    if let Some(video) = &msg.video {
        let text = msg.caption.clone().unwrap_or_else(|| "(video)".into());
        return (
            text,
            Some(AttachmentMeta {
                kind: "video".into(),
                file_id: video.file_id.clone(),
                size: video.file_size,
                mime: video.mime_type.clone(),
                name: video.file_name.as_deref().map(access::safe_name),
            }),
            false,
        );
    }
    if let Some(vn) = &msg.video_note {
        return (
            "(video note)".into(),
            Some(AttachmentMeta {
                kind: "video_note".into(),
                file_id: vn.file_id.clone(),
                size: vn.file_size,
                mime: None,
                name: None,
            }),
            false,
        );
    }
    if let Some(sticker) = &msg.sticker {
        let emoji = sticker
            .emoji
            .as_deref()
            .map(|e| format!(" {e}"))
            .unwrap_or_default();
        return (
            format!("(sticker{emoji})"),
            Some(AttachmentMeta {
                kind: "sticker".into(),
                file_id: sticker.file_id.clone(),
                size: sticker.file_size,
                mime: None,
                name: None,
            }),
            false,
        );
    }

    // Plain text.
    let text = msg.text.clone().unwrap_or_default();
    (text, None, false)
}

async fn download_photo_from_message(msg: &Message, ctx: &HandlerContext) -> Option<String> {
    let photos = msg.photo.as_ref()?;
    let best = photos.last()?;
    match ctx.api.get_file(&best.file_id).await {
        Ok(file) => {
            let file_path = file.file_path.as_deref()?;
            match ctx.api.download_file(file_path).await {
                Ok(bytes) => {
                    let ext = file_path.split('.').next_back().unwrap_or("jpg");
                    // Sanitize file_unique_id to prevent path traversal.
                    let sanitized_id: String = best
                        .file_unique_id
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                        .collect();
                    let sanitized_id = if sanitized_id.is_empty() {
                        "photo".to_string()
                    } else {
                        sanitized_id
                    };
                    let name = format!("{}-{}.{ext}", epoch_ms(), sanitized_id);
                    let path = ctx.inbox_dir.join(&name);
                    if let Err(e) = std::fs::create_dir_all(&ctx.inbox_dir) {
                        error!(error = %e, "failed to create inbox dir");
                        return None;
                    }
                    if let Err(e) = std::fs::write(&path, &bytes) {
                        error!(error = %e, "failed to write photo");
                        return None;
                    }
                    Some(path.to_string_lossy().into_owned())
                }
                Err(e) => {
                    warn!(error = %e, "photo download failed");
                    None
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "getFile for photo failed");
            None
        }
    }
}

fn epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Extract a substring from `text` using UTF-16 code-unit offsets (as
/// Telegram entity offsets are encoded). Returns `None` if the offsets
/// fall outside the string.
fn utf16_slice(text: &str, offset: usize, length: usize) -> Option<&str> {
    let mut byte_start = None;
    let mut utf16_pos: usize = 0;
    let target_end = offset + length;

    for (byte_idx, ch) in text.char_indices() {
        if utf16_pos == offset {
            byte_start = Some(byte_idx);
        }
        if utf16_pos == target_end {
            return Some(&text[byte_start?..byte_idx]);
        }
        utf16_pos += ch.len_utf16();
    }
    // Handle the case where target_end lands exactly at the string end.
    if utf16_pos == target_end {
        return Some(&text[byte_start?..]);
    }
    None
}

/// Check if the bot is @mentioned in a message.
fn check_mention(msg: &Message, bot_username: &str) -> bool {
    let entities = if !msg.entities.is_empty() {
        &msg.entities
    } else {
        &msg.caption_entities
    };
    let text = msg.text.as_deref().or(msg.caption.as_deref()).unwrap_or("");

    for e in entities {
        if e.kind == "mention" {
            if let Some(mentioned) = utf16_slice(text, e.offset, e.length) {
                if mentioned.eq_ignore_ascii_case(&format!("@{bot_username}")) {
                    return true;
                }
            }
        }
        if e.kind == "text_mention" {
            if let Some(ref user) = e.user {
                if user.is_bot == Some(true) && user.username.as_deref() == Some(bot_username) {
                    return true;
                }
            }
        }
    }

    // Reply to bot counts as implicit mention.
    if let Some(ref reply) = msg.reply_to_message {
        if let Some(ref reply_from) = reply.from {
            if reply_from.username.as_deref() == Some(bot_username) {
                return true;
            }
        }
    }

    false
}

/// Handle bot commands (/start, /help, /status).
async fn handle_command(msg: &Message, text: &str, ctx: &HandlerContext) {
    let chat_id = msg.chat.id.to_string();
    let cmd = text.split_whitespace().next().unwrap_or("");
    let cmd = cmd.split('@').next().unwrap_or(cmd); // strip @botname suffix

    match cmd {
        "/start" => {
            let access = access::load_access(&ctx.state_dir);
            let reply = if access.dm_policy == access::DmPolicy::Disabled {
                "This bot isn't accepting new connections.".to_string()
            } else {
                "This bot bridges Telegram to a Claude Code session.\n\n\
                 To pair:\n\
                 1. DM me anything \u{2014} you'll get a 6-char code\n\
                 2. In Claude Code: /telegram:access pair <code>\n\n\
                 After that, DMs here reach that session."
                    .to_string()
            };
            let _ = ctx
                .api
                .send_message(&chat_id, &reply, None, None, None)
                .await;
        }
        "/help" => {
            let reply = "Messages you send here route to a paired Claude Code session. \
                         Text and photos are forwarded; replies and reactions come back.\n\n\
                         /start \u{2014} pairing instructions\n\
                         /status \u{2014} check your pairing state";
            let _ = ctx
                .api
                .send_message(&chat_id, reply, None, None, None)
                .await;
        }
        "/status" => {
            let from = match msg.from.as_ref() {
                Some(f) => f,
                None => return,
            };
            let sender_id = from.id.to_string();
            let access = access::load_access(&ctx.state_dir);

            let reply = if access.allow_from.contains(&sender_id) {
                let name = from
                    .username
                    .as_ref()
                    .map(|u| format!("@{u}"))
                    .unwrap_or(sender_id);
                format!("Paired as {name}.")
            } else {
                // Check pending.
                let mut found = None;
                for (code, p) in &access.pending {
                    if p.sender_id == sender_id {
                        found = Some(code.clone());
                        break;
                    }
                }
                if let Some(code) = found {
                    format!(
                        "Pending pairing \u{2014} run in Claude Code:\n\n/telegram:access pair {code}"
                    )
                } else {
                    "Not paired. Send me a message to get a pairing code.".to_string()
                }
            };
            let _ = ctx
                .api
                .send_message(&chat_id, &reply, None, None, None)
                .await;
        }
        _ => {
            // Unknown command — ignore silently.
        }
    }
}

/// Handle callback queries (inline keyboard button presses for permissions).
///
/// Any paired user (present in `allow_from`) has permission-approval authority.
/// This matches the official TS plugin's behavior, which also broadcasts
/// permission prompts to ALL allowlisted users and accepts a verdict from
/// any of them. In multi-user setups, every paired user is equally trusted
/// to approve or deny tool-use requests.
async fn handle_callback_query(
    cb: &super::types::CallbackQuery,
    ctx: &HandlerContext,
) -> Option<Vec<Value>> {
    let data = cb.data.as_deref()?;

    // Parse perm:allow:<id>, perm:deny:<id>, perm:more:<id>
    static RE: LazyLock<regex_lite::Regex> = LazyLock::new(|| {
        regex_lite::Regex::new(r"^perm:(allow|deny|more):([a-km-z]{5})$").unwrap()
    });
    let re = &*RE;
    let caps = match re.captures(data) {
        Some(c) => c,
        None => {
            let _ = ctx.api.answer_callback_query(&cb.id, None).await;
            return None;
        }
    };

    let access = access::load_access(&ctx.state_dir);
    let sender_id = cb.from.id.to_string();
    if !access.allow_from.contains(&sender_id) {
        let _ = ctx
            .api
            .answer_callback_query(&cb.id, Some("Not authorized."))
            .await;
        return None;
    }

    let behavior = &caps[1];
    let request_id = &caps[2];

    if behavior == "more" {
        // "See more" — we don't have the full details in this binary
        // (they live in-memory in the TS plugin). Just acknowledge.
        let _ = ctx
            .api
            .answer_callback_query(&cb.id, Some("Details not available in this session."))
            .await;
        return None;
    }

    // Emit permission verdict.
    let frame = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel/permission",
        "params": {
            "request_id": request_id,
            "behavior": behavior,
        }
    });

    let label = if behavior == "allow" {
        "\u{2705} Allowed"
    } else {
        "\u{274c} Denied"
    };
    let _ = ctx.api.answer_callback_query(&cb.id, Some(label)).await;

    // Update the message to show the outcome.
    if let Some(ref cb_msg) = cb.message {
        let chat_id = cb_msg.chat.id.to_string();
        let old_text = cb_msg.text.as_deref().unwrap_or("");
        let new_text = format!("{old_text}\n\n{label}");
        let _ = ctx
            .api
            .edit_message_text_with_markup(&chat_id, cb_msg.message_id, &new_text, None)
            .await;
    }

    Some(vec![frame])
}

// ---------------------------------------------------------------------------
// Voice transcription helpers
// ---------------------------------------------------------------------------

/// Handle voice message transcription. Returns `Some(vec![])` (empty frames)
/// if the transcript was sent for echo-confirmation and the caller should NOT
/// emit a channel notification yet. Returns `None` if the text was updated
/// in-place and the caller should continue with normal delivery.
async fn handle_voice_transcription(
    msg: &Message,
    text: &mut String,
    attachment: &Option<AttachmentMeta>,
    chat_id: &str,
    ctx: &HandlerContext,
) -> Option<Vec<Value>> {
    let from = msg.from.as_ref()?;
    let sender_id = from.id.to_string();

    if !ctx.transcribe_support.available {
        // Whisper not installed — update text with a note.
        *text = msg.caption.clone().unwrap_or_else(|| {
            "(voice message \u{2014} transcription unavailable, install whisper + ffmpeg)".into()
        });
        return None;
    }

    // Download the voice file.
    let voice = msg.voice.as_ref()?;
    let ogg_path = match download_voice_file(&voice.file_id, ctx).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "failed to download voice file for transcription");
            return None;
        }
    };

    // Transcribe.
    let transcription = match super::transcribe::transcribe(&ogg_path, &ctx.transcribe_config).await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "voice transcription failed");
            *text = format!("(voice message \u{2014} transcription failed: {})", e);
            // Clean up downloaded file.
            let _ = tokio::fs::remove_file(&ogg_path).await;
            return None;
        }
    };

    // Clean up downloaded OGG file.
    let _ = tokio::fs::remove_file(&ogg_path).await;

    info!(
        chat_id,
        user_id = %sender_id,
        chars = transcription.len(),
        "voice transcription complete"
    );

    if ctx.transcribe_config.echo_transcript {
        // Send transcript back to chat for confirmation.
        let echo_text = format!(
            "\u{1f3a4} Transkrypcja:\n\n\"{transcription}\"\n\nOdpowiedz 'ok' aby potwierdzi\u{0107} lub popraw tekst."
        );
        let _ = ctx
            .api
            .send_message(chat_id, &echo_text, Some(msg.message_id), None, None)
            .await;

        // Store pending transcription.
        let pending = PendingTranscription {
            transcription,
            chat_id: chat_id.to_string(),
            original_message_id: msg.message_id.to_string(),
            user_id: sender_id.clone(),
            created_at: Instant::now(),
            attachment_meta: attachment.clone(),
        };
        let key = (chat_id.to_string(), sender_id);
        ctx.pending_transcriptions.lock().await.insert(key, pending);

        // Return empty frames — no channel notification yet.
        Some(vec![])
    } else {
        // No echo — deliver transcription directly.
        *text = transcription;
        None
    }
}

/// Check if a user message is a confirmation for a pending transcription.
/// Returns channel notification frames if it is, None otherwise.
async fn check_transcription_confirmation(
    text: &str,
    chat_id: &str,
    sender_id: &str,
    ctx: &HandlerContext,
) -> Option<Vec<Value>> {
    let key = (chat_id.to_string(), sender_id.to_string());

    let pending = {
        let mut map = ctx.pending_transcriptions.lock().await;

        // Expire old entries while we have the lock.
        map.retain(|_, p| p.created_at.elapsed() < PENDING_TRANSCRIPTION_TIMEOUT);

        map.remove(&key)?
    };

    let trimmed = text.trim().to_lowercase();
    let is_confirm = matches!(trimmed.as_str(), "ok" | "tak" | "yes" | "\u{1f44d}");

    let final_text = if is_confirm {
        pending.transcription.clone()
    } else {
        // User sent corrected text — use that instead.
        text.trim().to_string()
    };

    // React to confirm receipt.
    let _ = ctx
        .api
        .set_message_reaction(chat_id, 0, "\u{2705}") // We don't have the confirmation msg_id easily, skip reaction
        .await;

    // Build the channel notification frame.
    let mut meta = serde_json::Map::new();
    meta.insert("chat_id".into(), json!(pending.chat_id));
    meta.insert("message_id".into(), json!(pending.original_message_id));
    meta.insert("user_id".into(), json!(pending.user_id));
    if let Some(ref att) = pending.attachment_meta {
        meta.insert("attachment_kind".into(), json!(att.kind));
        meta.insert("attachment_file_id".into(), json!(att.file_id));
        if let Some(size) = att.size {
            meta.insert("attachment_size".into(), json!(size.to_string()));
        }
        if let Some(ref mime) = att.mime {
            meta.insert("attachment_mime".into(), json!(mime));
        }
    }

    let frame = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": final_text,
            "meta": Value::Object(meta),
        }
    });

    Some(vec![frame])
}

/// Download a voice file from Telegram to a local temp path.
async fn download_voice_file(file_id: &str, ctx: &HandlerContext) -> anyhow::Result<PathBuf> {
    let file = ctx.api.get_file(file_id).await?;
    let file_path = file
        .file_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Telegram returned no file_path for voice"))?;
    let bytes = ctx.api.download_file(file_path).await?;

    let ext = file_path.split('.').next_back().unwrap_or("oga");
    let id = uuid::Uuid::new_v4();
    let temp_path = std::env::temp_dir().join(format!("hdcd-voice-dl-{id}.{ext}"));

    tokio::fs::write(&temp_path, &bytes).await?;
    Ok(temp_path)
}

/// Spawn a background task that expires pending transcriptions after timeout,
/// emitting them as channel notifications with a note.
pub fn spawn_transcription_expiry(
    ctx: Arc<HandlerContext>,
    stdout: Arc<Mutex<tokio::io::Stdout>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
                _ = cancel.cancelled() => return,
            }

            let expired: Vec<PendingTranscription> = {
                let mut map = ctx.pending_transcriptions.lock().await;
                let mut expired = Vec::new();
                map.retain(|_, p| {
                    if p.created_at.elapsed() >= PENDING_TRANSCRIPTION_TIMEOUT {
                        expired.push(p.clone());
                        false
                    } else {
                        true
                    }
                });
                expired
            };

            for pending in expired {
                info!(
                    chat_id = %pending.chat_id,
                    user_id = %pending.user_id,
                    "pending transcription timed out, emitting as-is"
                );
                let content = format!("{} (auto-confirmed after timeout)", pending.transcription);
                let mut meta = serde_json::Map::new();
                meta.insert("chat_id".into(), json!(pending.chat_id));
                meta.insert("message_id".into(), json!(pending.original_message_id));
                meta.insert("user_id".into(), json!(pending.user_id));
                if let Some(ref att) = pending.attachment_meta {
                    meta.insert("attachment_kind".into(), json!(att.kind));
                    meta.insert("attachment_file_id".into(), json!(att.file_id));
                }

                let frame = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/claude/channel",
                    "params": {
                        "content": content,
                        "meta": Value::Object(meta),
                    }
                });

                let mut buf = match serde_json::to_vec(&frame) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                buf.push(b'\n');
                let mut out = stdout.lock().await;
                let _ = tokio::io::AsyncWriteExt::write_all(&mut *out, &buf).await;
                let _ = tokio::io::AsyncWriteExt::flush(&mut *out).await;
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Tool helpers (used by the tools module)
// ---------------------------------------------------------------------------

/// Maximum attachment size (50 MB).
pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;

/// Photo extensions that go as `sendPhoto` (inline preview).
const PHOTO_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp"];

pub fn is_photo_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| PHOTO_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Send a reply with chunking and optional file attachments.
pub async fn send_reply(
    api: &BotApi,
    access: &Access,
    chat_id: &str,
    text: &str,
    reply_to: Option<i64>,
    files: &[String],
    parse_mode: Option<&str>,
) -> anyhow::Result<Vec<i64>> {
    let limit = access.text_chunk_limit.unwrap_or(4096).clamp(1, 4096);
    let mode = access.chunk_mode.unwrap_or(ChunkMode::Length);
    let reply_mode = access.reply_to_mode.unwrap_or(ReplyToMode::First);

    let chunks = access::chunk_text(text, limit, mode);
    let mut sent_ids = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        let should_reply_to = reply_to.is_some()
            && reply_mode != ReplyToMode::Off
            && (reply_mode == ReplyToMode::All || i == 0);
        let rt = if should_reply_to { reply_to } else { None };
        let msg = api
            .send_message(chat_id, chunk, rt, parse_mode, None)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "reply failed after {} of {} chunk(s) sent: {}",
                    sent_ids.len(),
                    chunks.len(),
                    e
                )
            })?;
        sent_ids.push(msg.message_id);
    }

    // Files as separate messages.
    for f in files {
        let path = Path::new(f);
        let rt = if reply_to.is_some() && reply_mode != ReplyToMode::Off {
            reply_to
        } else {
            None
        };
        if is_photo_ext(path) {
            let msg = api.send_photo(chat_id, path, rt).await?;
            sent_ids.push(msg.message_id);
        } else {
            let msg = api.send_document(chat_id, path, rt).await?;
            sent_ids.push(msg.message_id);
        }
    }

    Ok(sent_ids)
}
