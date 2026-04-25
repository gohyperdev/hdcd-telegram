// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Permission relay — receive `permission_request` from Claude Code,
//! send inline keyboard to allowlisted DMs, relay verdict back.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tracing::warn;

use super::access;
use super::api::BotApi;
use super::types::InlineKeyboardButton;
use super::types::InlineKeyboardMarkup;

/// Handle a `notifications/claude/channel/permission_request` from Claude Code.
/// Sends an inline keyboard to all allowFrom DMs.
pub async fn handle_permission_request(params: &Value, api: &Arc<BotApi>, state_dir: &Path) {
    let request_id = match params.get("request_id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return,
    };
    let tool_name = params
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let access_data = access::load_access(state_dir);
    let text = format!("\u{1f510} Permission: {tool_name}");
    let keyboard = InlineKeyboardMarkup {
        inline_keyboard: vec![vec![
            InlineKeyboardButton {
                text: "See more".into(),
                callback_data: Some(format!("perm:more:{request_id}")),
            },
            InlineKeyboardButton {
                text: "\u{2705} Allow".into(),
                callback_data: Some(format!("perm:allow:{request_id}")),
            },
            InlineKeyboardButton {
                text: "\u{274c} Deny".into(),
                callback_data: Some(format!("perm:deny:{request_id}")),
            },
        ]],
    };

    for chat_id in &access_data.allow_from {
        if let Err(e) = api
            .send_message(chat_id, &text, None, None, Some(&keyboard), None)
            .await
        {
            warn!(chat_id, error = %e, "failed to send permission request");
        }
    }
}
