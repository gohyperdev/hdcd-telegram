// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! MCP tool schemas and call handlers for the Telegram channel.
//!
//! Tools: `reply`, `react`, `edit_message`, `download_attachment`,
//! `set_topic_title` (router mode only).

use std::path::Path;

use anyhow::Result;
use serde_json::{json, Value};

use super::access;
use super::api::BotApi;
use super::handlers;

/// Return the tool schemas for `tools/list`.
pub fn tool_schemas() -> Value {
    json!([
        {
            "name": "reply",
            "description": "Reply on Telegram. Pass chat_id from the inbound message. Optionally pass reply_to (message_id) for threading, and files (absolute paths) to attach images or documents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": { "type": "string" },
                    "text": { "type": "string" },
                    "reply_to": {
                        "type": "string",
                        "description": "Message ID to thread under. Use message_id from the inbound <channel> block."
                    },
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Absolute file paths to attach. Images send as photos (inline preview); other types as documents. Max 50MB each."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["text", "markdownv2"],
                        "description": "Rendering mode. 'markdownv2' enables Telegram formatting (bold, italic, code, links). Caller must escape special chars per MarkdownV2 rules. Default: 'text' (plain, no escaping needed)."
                    }
                },
                "required": ["chat_id", "text"]
            }
        },
        {
            "name": "react",
            "description": "Add an emoji reaction to a Telegram message. Telegram only accepts a fixed whitelist (\u{1f44d} \u{1f44e} \u{2764} \u{1f525} \u{1f440} \u{1f389} etc) \u{2014} non-whitelisted emoji will be rejected.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "emoji": { "type": "string" }
                },
                "required": ["chat_id", "message_id", "emoji"]
            }
        },
        {
            "name": "download_attachment",
            "description": "Download a file attachment from a Telegram message to the local inbox. Use when the inbound <channel> meta shows attachment_file_id. Returns the local file path ready to Read. Telegram caps bot downloads at 20MB.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "The attachment_file_id from inbound meta" }
                },
                "required": ["file_id"]
            }
        },
        {
            "name": "edit_message",
            "description": "Edit a message the bot previously sent. Useful for interim progress updates. Edits don't trigger push notifications \u{2014} send a new reply when a long task completes so the user's device pings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "chat_id": { "type": "string" },
                    "message_id": { "type": "string" },
                    "text": { "type": "string" },
                    "format": {
                        "type": "string",
                        "enum": ["text", "markdownv2"],
                        "description": "Rendering mode. 'markdownv2' enables Telegram formatting (bold, italic, code, links). Caller must escape special chars per MarkdownV2 rules. Default: 'text' (plain, no escaping needed)."
                    }
                },
                "required": ["chat_id", "message_id", "text"]
            }
        },
        {
            "name": "set_topic_title",
            "description": "Rename this session's Telegram forum topic. Call when the conversation subject becomes clear or shifts significantly \u{2014} e.g. after the first real task is understood, or when switching to a new subject. Keep titles short (2\u{2013}5 words) and descriptive so the user can find this session in their topic sidebar. Don't rename for minor follow-ups. Router mode only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "New topic name, 1\u{2013}128 characters."
                    }
                },
                "required": ["title"]
            }
        }
    ])
}

/// Handle a `tools/call` request. Returns the MCP response `result` value.
pub async fn handle_tool_call(
    name: &str,
    args: &Value,
    api: &BotApi,
    state_dir: &Path,
    inbox_dir: &Path,
) -> Result<Value> {
    match name {
        "reply" => handle_reply(args, api, state_dir).await,
        "react" => handle_react(args, api, state_dir).await,
        "download_attachment" => handle_download(args, api, inbox_dir).await,
        "edit_message" => handle_edit(args, api, state_dir).await,
        "set_topic_title" => {
            anyhow::bail!(
                "set_topic_title is only available in router mode — \
                 direct mode has no forum-topic concept"
            )
        }
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

async fn handle_reply(args: &Value, api: &BotApi, state_dir: &Path) -> Result<Value> {
    let chat_id = args["chat_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing chat_id"))?;
    let text = args["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing text"))?;
    let reply_to = args
        .get("reply_to")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<i64>().ok());
    let files: Vec<String> = args
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let format_str = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    let parse_mode = if format_str == "markdownv2" {
        Some("MarkdownV2")
    } else {
        None
    };

    access::assert_allowed_chat(state_dir, chat_id)?;

    // Validate files.
    for f in &files {
        assert_sendable(f, state_dir)?;
        let meta = std::fs::metadata(f).map_err(|e| anyhow::anyhow!("cannot stat {f}: {e}"))?;
        if meta.len() > handlers::MAX_ATTACHMENT_BYTES {
            anyhow::bail!(
                "file too large: {f} ({:.1}MB, max 50MB)",
                meta.len() as f64 / (1024.0 * 1024.0)
            );
        }
    }

    let access_data = access::load_access(state_dir);
    let sent_ids = handlers::send_reply(
        api,
        &access_data,
        chat_id,
        text,
        reply_to,
        &files,
        parse_mode,
    )
    .await?;

    let result = if sent_ids.len() == 1 {
        format!("sent (id: {})", sent_ids[0])
    } else {
        format!(
            "sent {} parts (ids: {})",
            sent_ids.len(),
            sent_ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(json!({ "content": [{ "type": "text", "text": result }] }))
}

async fn handle_react(args: &Value, api: &BotApi, state_dir: &Path) -> Result<Value> {
    let chat_id = args["chat_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing chat_id"))?;
    let message_id = args["message_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing message_id"))?
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("invalid message_id"))?;
    let emoji = args["emoji"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing emoji"))?;

    access::assert_allowed_chat(state_dir, chat_id)?;
    api.set_message_reaction(chat_id, message_id, emoji).await?;
    Ok(json!({ "content": [{ "type": "text", "text": "reacted" }] }))
}

async fn handle_download(args: &Value, api: &BotApi, inbox_dir: &Path) -> Result<Value> {
    let file_id = args["file_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing file_id"))?;

    let file = api.get_file(file_id).await?;
    let file_path = file.file_path.as_deref().ok_or_else(|| {
        anyhow::anyhow!("Telegram returned no file_path -- file may have expired")
    })?;

    let bytes = api.download_file(file_path).await?;

    let raw_ext = if file_path.contains('.') {
        file_path.split('.').next_back().unwrap_or("bin")
    } else {
        "bin"
    };
    let ext: String = raw_ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let ext = if ext.is_empty() {
        "bin".to_string()
    } else {
        ext
    };
    let unique_id: String = file
        .file_unique_id
        .as_deref()
        .unwrap_or("dl")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let unique_id = if unique_id.is_empty() {
        "dl".to_string()
    } else {
        unique_id
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = inbox_dir.join(format!("{now}-{unique_id}.{ext}"));

    std::fs::create_dir_all(inbox_dir)?;
    std::fs::write(&path, &bytes)?;

    let path_str = path.to_string_lossy().into_owned();
    Ok(json!({ "content": [{ "type": "text", "text": path_str }] }))
}

async fn handle_edit(args: &Value, api: &BotApi, state_dir: &Path) -> Result<Value> {
    let chat_id = args["chat_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing chat_id"))?;
    let message_id = args["message_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing message_id"))?
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("invalid message_id"))?;
    let text = args["text"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing text"))?;
    let format_str = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    let parse_mode = if format_str == "markdownv2" {
        Some("MarkdownV2")
    } else {
        None
    };

    access::assert_allowed_chat(state_dir, chat_id)?;
    let msg = api
        .edit_message_text(chat_id, message_id, text, parse_mode)
        .await?;
    Ok(
        json!({ "content": [{ "type": "text", "text": format!("edited (id: {})", msg.message_id) }] }),
    )
}

/// Prevent sending files from the state directory (except inbox).
/// The file must exist (canonicalize verifies this) to avoid TOCTOU issues.
fn assert_sendable(path: &str, state_dir: &Path) -> Result<()> {
    let real = std::fs::canonicalize(path)
        .map_err(|e| anyhow::anyhow!("file does not exist or is inaccessible: {path}: {e}"))?;
    let state_real = match std::fs::canonicalize(state_dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let inbox = state_real.join("inbox");
    if real.starts_with(&state_real) && !real.starts_with(&inbox) {
        anyhow::bail!("refusing to send channel state: {path}");
    }
    Ok(())
}
