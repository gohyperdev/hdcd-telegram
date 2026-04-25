// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Raw Telegram Bot API HTTP client.
//!
//! Uses `reqwest` directly — no Telegram SDK crate. Every method maps 1:1
//! to a Bot API endpoint. All methods are `async` and return `anyhow::Result`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use reqwest::multipart;
use serde::Serialize;
use serde_json::{json, Value};

use super::types::*;

/// Maximum download size for files from Telegram (20 MB — Telegram bot API limit).
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

/// Wraps a bot token so that it never leaks through `Debug` or `Display`.
#[derive(Clone)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Return the raw token string for URL construction.
    pub fn token_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::fmt::Display for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Lightweight handle to the Telegram Bot API. Cheap to clone (shares the
/// inner `reqwest::Client` via Arc).
#[derive(Clone)]
pub struct BotApi {
    token: SecretToken,
    client: reqwest::Client,
}

impl BotApi {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: SecretToken::new(token),
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.token.token_str(),
            method
        )
    }

    fn file_url(&self, file_path: &str) -> String {
        format!(
            "https://api.telegram.org/file/bot{}/{}",
            self.token.token_str(),
            file_path
        )
    }

    // ------------------------------------------------------------------
    // Bot info
    // ------------------------------------------------------------------

    pub async fn get_me(&self) -> Result<BotUser> {
        let resp: GetMeResponse = self
            .client
            .get(self.url("getMe"))
            .send()
            .await
            .context("getMe request")?
            .json()
            .await
            .context("getMe parse")?;
        resp.result.context("getMe: ok=false or missing result")
    }

    // ------------------------------------------------------------------
    // Updates (long-polling)
    // ------------------------------------------------------------------

    pub async fn get_updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<GetUpdatesResponse> {
        let mut body = json!({ "timeout": timeout_secs });
        if let Some(off) = offset {
            body["offset"] = json!(off);
        }
        body["allowed_updates"] = json!(["message", "callback_query"]);

        let resp = self
            .client
            .post(self.url("getUpdates"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(timeout_secs + 10))
            .send()
            .await
            .context("getUpdates request")?
            .json::<GetUpdatesResponse>()
            .await
            .context("getUpdates parse")?;
        Ok(resp)
    }

    // ------------------------------------------------------------------
    // Sending
    // ------------------------------------------------------------------

    pub async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
        reply_to: Option<i64>,
        parse_mode: Option<&str>,
        reply_markup: Option<&InlineKeyboardMarkup>,
        message_thread_id: Option<i64>,
    ) -> Result<Message> {
        let mut body = json!({
            "chat_id": chat_id,
            "text": text,
        });
        if let Some(thread_id) = message_thread_id {
            body["message_thread_id"] = json!(thread_id);
        }
        if let Some(rt) = reply_to {
            body["reply_parameters"] = json!({ "message_id": rt });
        }
        if let Some(pm) = parse_mode {
            body["parse_mode"] = json!(pm);
        }
        if let Some(markup) = reply_markup {
            body["reply_markup"] = serde_json::to_value(markup).context("serialize markup")?;
        }

        let resp: SendMessageResponse = self
            .client
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await
            .context("sendMessage request")?
            .json()
            .await
            .context("sendMessage parse")?;
        if !resp.ok {
            bail!(
                "sendMessage failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        resp.result.context("sendMessage: missing result")
    }

    pub async fn send_photo(
        &self,
        chat_id: &str,
        photo_path: &Path,
        reply_to: Option<i64>,
    ) -> Result<Message> {
        let file_bytes = tokio::fs::read(photo_path)
            .await
            .with_context(|| format!("read photo {}", photo_path.display()))?;
        let file_name = photo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("photo.jpg")
            .to_string();
        let part = multipart::Part::bytes(file_bytes).file_name(file_name);
        let mut form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", part);
        if let Some(rt) = reply_to {
            form = form.text("reply_parameters", json!({ "message_id": rt }).to_string());
        }

        let resp: SendMessageResponse = self
            .client
            .post(self.url("sendPhoto"))
            .multipart(form)
            .send()
            .await
            .context("sendPhoto request")?
            .json()
            .await
            .context("sendPhoto parse")?;
        if !resp.ok {
            bail!("sendPhoto failed: {}", resp.description.unwrap_or_default());
        }
        resp.result.context("sendPhoto: missing result")
    }

    pub async fn send_document(
        &self,
        chat_id: &str,
        doc_path: &Path,
        reply_to: Option<i64>,
    ) -> Result<Message> {
        let file_bytes = tokio::fs::read(doc_path)
            .await
            .with_context(|| format!("read document {}", doc_path.display()))?;
        let file_name = doc_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let part = multipart::Part::bytes(file_bytes).file_name(file_name);
        let mut form = multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", part);
        if let Some(rt) = reply_to {
            form = form.text("reply_parameters", json!({ "message_id": rt }).to_string());
        }

        let resp: SendMessageResponse = self
            .client
            .post(self.url("sendDocument"))
            .multipart(form)
            .send()
            .await
            .context("sendDocument request")?
            .json()
            .await
            .context("sendDocument parse")?;
        if !resp.ok {
            bail!(
                "sendDocument failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        resp.result.context("sendDocument: missing result")
    }

    // ------------------------------------------------------------------
    // Reactions / edits
    // ------------------------------------------------------------------

    pub async fn set_message_reaction(
        &self,
        chat_id: &str,
        message_id: i64,
        emoji: &str,
    ) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "reaction": [{ "type": "emoji", "emoji": emoji }],
        });
        let resp: GenericResponse = self
            .client
            .post(self.url("setMessageReaction"))
            .json(&body)
            .send()
            .await
            .context("setMessageReaction request")?
            .json()
            .await
            .context("setMessageReaction parse")?;
        if !resp.ok {
            bail!(
                "setMessageReaction failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn edit_message_text(
        &self,
        chat_id: &str,
        message_id: i64,
        text: &str,
        parse_mode: Option<&str>,
    ) -> Result<Message> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        if let Some(pm) = parse_mode {
            body["parse_mode"] = json!(pm);
        }
        let resp: SendMessageResponse = self
            .client
            .post(self.url("editMessageText"))
            .json(&body)
            .send()
            .await
            .context("editMessageText request")?
            .json()
            .await
            .context("editMessageText parse")?;
        if !resp.ok {
            bail!(
                "editMessageText failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        resp.result.context("editMessageText: missing result")
    }

    // ------------------------------------------------------------------
    // Files
    // ------------------------------------------------------------------

    pub async fn get_file(&self, file_id: &str) -> Result<File> {
        let body = json!({ "file_id": file_id });
        let resp: GetFileResponse = self
            .client
            .post(self.url("getFile"))
            .json(&body)
            .send()
            .await
            .context("getFile request")?
            .json()
            .await
            .context("getFile parse")?;
        if !resp.ok {
            bail!("getFile failed");
        }
        resp.result.context("getFile: missing result")
    }

    pub async fn download_file(&self, file_path: &str) -> Result<Vec<u8>> {
        let url = self.file_url(file_path);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("download file request")?;
        if !resp.status().is_success() {
            bail!("download failed: HTTP {}", resp.status());
        }

        // Check Content-Length header before downloading body.
        if let Some(cl) = resp.content_length() {
            if cl > MAX_DOWNLOAD_BYTES {
                bail!(
                    "file too large: {:.1}MB exceeds 20MB download limit",
                    cl as f64 / (1024.0 * 1024.0)
                );
            }
        }

        // Stream body with a running size counter to enforce the limit even
        // when Content-Length is absent.
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("download stream chunk")?;
            buf.extend_from_slice(&chunk);
            if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
                bail!("file too large: exceeded 20MB download limit while streaming");
            }
        }
        Ok(buf)
    }

    // ------------------------------------------------------------------
    // Chat actions / commands
    // ------------------------------------------------------------------

    pub async fn send_chat_action(&self, chat_id: &str, action: &str) -> Result<()> {
        let body = json!({ "chat_id": chat_id, "action": action });
        let _resp: GenericResponse = self
            .client
            .post(self.url("sendChatAction"))
            .json(&body)
            .send()
            .await
            .context("sendChatAction request")?
            .json()
            .await
            .context("sendChatAction parse")?;
        Ok(())
    }

    pub async fn set_my_commands(&self, commands: &[BotCommand], scope: Value) -> Result<()> {
        let body = json!({
            "commands": commands,
            "scope": scope,
        });
        let _resp: GenericResponse = self
            .client
            .post(self.url("setMyCommands"))
            .json(&body)
            .send()
            .await
            .context("setMyCommands request")?
            .json()
            .await
            .context("setMyCommands parse")?;
        Ok(())
    }

    pub async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<()> {
        let mut body = json!({ "callback_query_id": callback_query_id });
        if let Some(t) = text {
            body["text"] = json!(t);
        }
        let _resp: GenericResponse = self
            .client
            .post(self.url("answerCallbackQuery"))
            .json(&body)
            .send()
            .await
            .context("answerCallbackQuery request")?
            .json()
            .await
            .context("answerCallbackQuery parse")?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Forum topics
    // ------------------------------------------------------------------

    pub async fn create_forum_topic(
        &self,
        chat_id: &str,
        name: &str,
        icon_color: Option<i64>,
    ) -> Result<ForumTopic> {
        let mut body = json!({
            "chat_id": chat_id,
            "name": name,
        });
        if let Some(color) = icon_color {
            body["icon_color"] = json!(color);
        }
        let resp: CreateForumTopicResponse = self
            .client
            .post(self.url("createForumTopic"))
            .json(&body)
            .send()
            .await
            .context("createForumTopic request")?
            .json()
            .await
            .context("createForumTopic parse")?;
        if !resp.ok {
            bail!(
                "createForumTopic failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        resp.result.context("createForumTopic: missing result")
    }

    pub async fn close_forum_topic(&self, chat_id: &str, message_thread_id: i64) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_thread_id": message_thread_id,
        });
        let resp: GenericResponse = self
            .client
            .post(self.url("closeForumTopic"))
            .json(&body)
            .send()
            .await
            .context("closeForumTopic request")?
            .json()
            .await
            .context("closeForumTopic parse")?;
        if !resp.ok {
            bail!(
                "closeForumTopic failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn reopen_forum_topic(&self, chat_id: &str, message_thread_id: i64) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_thread_id": message_thread_id,
        });
        let resp: GenericResponse = self
            .client
            .post(self.url("reopenForumTopic"))
            .json(&body)
            .send()
            .await
            .context("reopenForumTopic request")?
            .json()
            .await
            .context("reopenForumTopic parse")?;
        if !resp.ok {
            bail!(
                "reopenForumTopic failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn delete_forum_topic(&self, chat_id: &str, message_thread_id: i64) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_thread_id": message_thread_id,
        });
        let resp: GenericResponse = self
            .client
            .post(self.url("deleteForumTopic"))
            .json(&body)
            .send()
            .await
            .context("deleteForumTopic request")?
            .json()
            .await
            .context("deleteForumTopic parse")?;
        if !resp.ok {
            bail!(
                "deleteForumTopic failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn edit_forum_topic(
        &self,
        chat_id: &str,
        message_thread_id: i64,
        name: &str,
    ) -> Result<()> {
        let body = json!({
            "chat_id": chat_id,
            "message_thread_id": message_thread_id,
            "name": name,
        });
        let resp: GenericResponse = self
            .client
            .post(self.url("editForumTopic"))
            .json(&body)
            .send()
            .await
            .context("editForumTopic request")?
            .json()
            .await
            .context("editForumTopic parse")?;
        if !resp.ok {
            bail!(
                "editForumTopic failed: {}",
                resp.description.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn edit_message_text_with_markup(
        &self,
        chat_id: &str,
        message_id: i64,
        text: &str,
        reply_markup: Option<&InlineKeyboardMarkup>,
    ) -> Result<()> {
        let mut body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        if let Some(markup) = reply_markup {
            body["reply_markup"] = serde_json::to_value(markup).context("serialize markup")?;
        }
        let _resp: GenericResponse = self
            .client
            .post(self.url("editMessageText"))
            .json(&body)
            .send()
            .await
            .context("editMessageText request")?
            .json()
            .await
            .context("editMessageText parse")?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BotCommand {
    pub command: String,
    pub description: String,
}
