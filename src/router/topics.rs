// SPDX-License-Identifier: Apache-2.0

//! Forum topic management — creates, closes, and reopens topics
//! in the configured supergroup, coordinating with the session registry.

use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::telegram::api::BotApi;

use super::config::RouterConfig;
use super::sessions::{Registration, SessionRegistry};

/// Default forum-topic icon color (Telegram's orange, 0xFB6F5F).
/// Base color can only be set at creation — editForumTopic accepts name + custom emoji only.
/// See https://core.telegram.org/bots/api#createforumtopic
const TOPIC_ICON_COLOR: i64 = 16478047;

/// Manages forum topics for the router.
pub struct TopicManager {
    api: Arc<BotApi>,
    supergroup_id: String,
    close_on_disconnect: bool,
}

impl TopicManager {
    pub fn new(api: Arc<BotApi>, config: &RouterConfig) -> Self {
        Self {
            api,
            supergroup_id: config.supergroup_id.clone(),
            close_on_disconnect: config.close_topic_on_disconnect,
        }
    }

    /// Get a reference to the underlying API client.
    pub fn api(&self) -> &Arc<BotApi> {
        &self.api
    }

    /// Create a new forum topic for a session.
    ///
    /// Returns the `message_thread_id` (topic ID).
    pub async fn ensure_topic(
        &self,
        session_id: &str,
        reg: &Registration,
        registry: &mut SessionRegistry,
    ) -> Result<i64> {
        // Check if session already has a topic.
        if let Some(topic_id) = registry.topic_by_session(session_id) {
            return Ok(topic_id);
        }

        // Always create a new topic — each session is independent.
        let topic = self
            .api
            .create_forum_topic(&self.supergroup_id, &reg.label, Some(TOPIC_ICON_COLOR))
            .await?;

        let topic_id = topic.message_thread_id;
        info!(
            session_id,
            topic_id,
            label = %reg.label,
            "created forum topic"
        );

        registry.register(session_id, topic_id, reg);
        self.send_welcome(topic_id, reg).await;

        Ok(topic_id)
    }

    /// Close the forum topic for a session.
    pub async fn close_topic(
        &self,
        session_id: &str,
        registry: &mut SessionRegistry,
    ) {
        if !self.close_on_disconnect {
            return;
        }

        let topic_id = match registry.topic_by_session(session_id) {
            Some(id) => id,
            None => return,
        };

        // Send disconnect message before closing.
        let entry = registry.sessions.get(session_id);
        let label = entry.map(|e| e.label.as_str()).unwrap_or("?");
        let disconnect_text = format!("Session disconnected: {label}");
        let _ = self
            .api
            .send_message(
                &self.supergroup_id,
                &disconnect_text,
                None,
                None,
                None,
                Some(topic_id),
            )
            .await;

        if let Err(e) = self
            .api
            .close_forum_topic(&self.supergroup_id, topic_id)
            .await
        {
            warn!(error = %e, topic_id, "failed to close topic");
        } else {
            info!(session_id, topic_id, "closed forum topic");
        }

        registry.close(session_id);
    }

    /// Send a welcome message to a newly opened/reopened topic.
    async fn send_welcome(&self, topic_id: i64, reg: &Registration) {
        let pid_info = reg
            .pid
            .map(|p| format!(" (PID {p})"))
            .unwrap_or_default();
        let text = format!("Session connected: {}{pid_info}", reg.label);
        let _ = self
            .api
            .send_message(
                &self.supergroup_id,
                &text,
                None,
                None,
                None,
                Some(topic_id),
            )
            .await;
    }

    /// Rename a session's forum topic. Returns Ok even if the title didn't
    /// change — only logs a warning on API errors to avoid blocking the
    /// outbox pipeline on a transient Telegram failure.
    pub async fn rename_topic(
        &self,
        session_id: &str,
        new_title: &str,
        registry: &mut SessionRegistry,
    ) {
        let topic_id = match registry.topic_by_session(session_id) {
            Some(id) => id,
            None => {
                warn!(session_id, "rename_topic: no topic for session");
                return;
            }
        };

        if !registry.set_title(session_id, new_title) {
            return; // unchanged — skip API call
        }

        match self
            .api
            .edit_forum_topic(&self.supergroup_id, topic_id, new_title)
            .await
        {
            Ok(()) => info!(session_id, topic_id, title = %new_title, "renamed topic"),
            Err(e) => warn!(session_id, topic_id, error = %e, "editForumTopic failed"),
        }
    }

    /// Send a message to a session's topic.
    pub async fn send_to_topic(
        &self,
        topic_id: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<i64> {
        let msg = self
            .api
            .send_message(
                &self.supergroup_id,
                text,
                reply_to,
                None,
                None,
                Some(topic_id),
            )
            .await?;
        Ok(msg.message_id)
    }
}

