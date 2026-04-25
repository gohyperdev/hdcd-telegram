// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Telegram Bot API types.
//!
//! Only the subset needed by this channel module. Field names match the
//! Telegram Bot API JSON exactly (snake_case).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// getUpdates
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct GetUpdatesResponse {
    pub ok: bool,
    #[serde(default)]
    pub result: Vec<Update>,
    pub description: Option<String>,
    pub error_code: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<CallbackQuery>,
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub from: Option<User>,
    pub chat: Chat,
    #[serde(default)]
    pub date: i64,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub photo: Option<Vec<PhotoSize>>,
    pub document: Option<Document>,
    pub voice: Option<Voice>,
    pub audio: Option<Audio>,
    pub video: Option<Video>,
    pub video_note: Option<VideoNote>,
    pub sticker: Option<Sticker>,
    pub message_thread_id: Option<i64>,
    pub reply_to_message: Option<Box<Message>>,
    #[serde(default)]
    pub entities: Vec<MessageEntity>,
    #[serde(default)]
    pub caption_entities: Vec<MessageEntity>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageEntity {
    #[serde(rename = "type")]
    pub kind: String,
    pub offset: usize,
    pub length: usize,
    pub user: Option<User>,
}

// ---------------------------------------------------------------------------
// Chat / User
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub chat_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    pub is_bot: Option<bool>,
    pub username: Option<String>,
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct PhotoSize {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Document {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Voice {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub file_name: Option<String>,
    pub title: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Video {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub file_name: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VideoNote {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub file_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sticker {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub emoji: Option<String>,
    pub file_size: Option<i64>,
}

// ---------------------------------------------------------------------------
// Callback query
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    pub message: Option<Message>,
    pub data: Option<String>,
}

// ---------------------------------------------------------------------------
// getFile
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct GetFileResponse {
    pub ok: bool,
    pub result: Option<File>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct File {
    pub file_id: String,
    pub file_unique_id: Option<String>,
    pub file_path: Option<String>,
}

// ---------------------------------------------------------------------------
// getMe
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct GetMeResponse {
    pub ok: bool,
    pub result: Option<BotUser>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotUser {
    pub id: i64,
    pub username: Option<String>,
}

// ---------------------------------------------------------------------------
// sendMessage result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageResponse {
    pub ok: bool,
    pub result: Option<Message>,
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Generic OK response (for setMessageReaction, etc.)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct GenericResponse {
    pub ok: bool,
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Forum topics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct ForumTopic {
    pub message_thread_id: i64,
    pub name: String,
    pub icon_color: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateForumTopicResponse {
    pub ok: bool,
    pub result: Option<ForumTopic>,
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// Inline keyboard
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct InlineKeyboardMarkup {
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InlineKeyboardButton {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_data: Option<String>,
}
