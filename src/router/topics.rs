// SPDX-License-Identifier: Apache-2.0

//! Forum topic management — creates, closes, reopens, and rebinds topics
//! in the configured supergroup, coordinating with the session registry.

use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::telegram::api::BotApi;

use super::config::RouterConfig;
use super::sessions::{Registration, SessionRegistry};

/// Default forum-topic icon color (Telegram's yellow, 0xFFD67E — renders
/// as orange in the client). Base color can only be set at creation —
/// editForumTopic accepts name + custom emoji only.
/// See https://core.telegram.org/bots/api#createforumtopic
const TOPIC_ICON_COLOR: i64 = 16766590;

/// Manages forum topics for the router.
pub struct TopicManager {
    api: Arc<BotApi>,
    /// String form of the supergroup chat id, cached at construction
    /// because every Bot API call here wants `chat_id: &str`.
    supergroup_id: String,
    close_on_disconnect: bool,
}

impl TopicManager {
    pub fn new(api: Arc<BotApi>, config: &RouterConfig) -> Self {
        Self {
            api,
            supergroup_id: config.chat_id_str(),
            close_on_disconnect: config.close_topic_on_disconnect,
        }
    }

    /// Get a reference to the underlying API client.
    pub fn api(&self) -> &Arc<BotApi> {
        &self.api
    }

    /// Reconcile a session's topic with its current registration. Handles
    /// three cases in one flow:
    ///
    /// - **First registration:** no entry yet → create or reopen an
    ///   existing topic for this `claude_session_id`, bind to it.
    /// - **Rebind (sessionId changed):** entry exists but `claude_session_id`
    ///   has shifted (e.g. user picked a `--resume` target, `/resume`, or
    ///   `/clear`) → dispose of the old topic (delete if it never saw
    ///   traffic, close otherwise), attach to the new one.
    /// - **No-op:** entry already matches registration → return current topic.
    ///
    /// Returns the bound `message_thread_id`.
    pub async fn reconcile_session(
        &self,
        reg: &Registration,
        registry: &mut SessionRegistry,
    ) -> Result<i64> {
        let session_id = reg.session_id.as_str();
        let current_topic = registry.topic_by_session(session_id);
        let current_claude_id = registry
            .claude_session_id_of(session_id)
            .map(str::to_string);

        if current_topic.is_some() && current_claude_id == reg.claude_session_id {
            return Ok(current_topic.unwrap());
        }

        let existing_for_claude = reg
            .claude_session_id
            .as_deref()
            .and_then(|id| registry.find_by_claude_session(id))
            .map(|(sid, tid)| (sid.to_string(), tid));

        // Probe the candidate for reattachment. reopen returns
        // TOPIC_NOT_MODIFIED if it's already open (fine), TOPIC_ID_INVALID
        // if Telegram has lost or deleted the topic (stale registry entry
        // — forget it and fall through to create a fresh one). Any other
        // failure is transient; keep the candidate and hope for the best.
        let mut reattached_topic = None;
        let mut stale_entry_for_claude: Option<String> = None;
        if let Some((stale_sid, t)) = existing_for_claude {
            match self.api.reopen_forum_topic(&self.supergroup_id, t).await {
                Ok(()) => reattached_topic = Some(t),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("TOPIC_ID_INVALID") {
                        warn!(
                            topic_id = t,
                            "candidate topic no longer exists on Telegram — discarding stale registry entry and creating new topic"
                        );
                        stale_entry_for_claude = Some(stale_sid);
                    } else {
                        warn!(
                            error = %msg,
                            topic_id = t,
                            "reopen_forum_topic failed — topic may already be open"
                        );
                        reattached_topic = Some(t);
                    }
                }
            }
        }

        if let Some(stale) = stale_entry_for_claude {
            registry.forget(&stale);
        }

        // No prior topic for this claude_session_id. Two sub-cases fall
        // here, distinguished by whether the current topic has seen any
        // traffic:
        //
        // - **Picker → real transition (activity == 0):** MCP just
        //   started, bound to a fresh topic under the picker-phase
        //   sessionId; watcher has now rewritten the registration with
        //   the real id. Reuse the current topic — creating a second one
        //   would flash/delete the first one the user already saw
        //   appear in the chat.
        // - **Mid-session switch (activity > 0):** user did `/resume` or
        //   `/clear` and the new sessionId is one the router has never
        //   seen. The current topic already holds history for a
        //   *different* logical session — don't hijack it. Retire it
        //   below (closes on non-zero activity, preserving history) and
        //   create a fresh topic for the new claude_session_id.
        let target_topic = match reattached_topic {
            Some(t) => t,
            None => match current_topic {
                Some(current) if registry.activity_count(session_id) == 0 => current,
                _ => {
                    let topic = self
                        .api
                        .create_forum_topic(
                            &self.supergroup_id,
                            &reg.label,
                            Some(TOPIC_ICON_COLOR),
                        )
                        .await?;
                    topic.message_thread_id
                }
            },
        };

        if let Some(old) = current_topic {
            if old != target_topic {
                let activity = registry.activity_count(session_id);
                self.retire_topic(old, activity).await;
            }
        }

        registry.bind(session_id, target_topic, reg);

        info!(
            session_id,
            topic_id = target_topic,
            claude_session_id = reg.claude_session_id.as_deref().unwrap_or("?"),
            label = %reg.label,
            reattached = reattached_topic.is_some(),
            "session bound to topic"
        );

        Ok(target_topic)
    }

    /// Close a session's topic on disconnect. Registry state always
    /// updates; the actual API call is gated by config. Paraysitic
    /// topics (zero activity — picker stubs, aborted resumes, crashed
    /// MCPs that never got a real message) are deleted instead of
    /// closed, matching the rebind path's `retire_topic` behavior so the
    /// supergroup doesn't accumulate empty closed stubs.
    pub async fn close_topic(&self, session_id: &str, registry: &mut SessionRegistry) {
        let topic_id = match registry.topic_by_session(session_id) {
            Some(id) => id,
            None => return,
        };
        let activity = registry.activity_count(session_id);

        let deleted = if self.close_on_disconnect {
            self.retire_topic(topic_id, activity).await
        } else {
            false
        };

        if deleted {
            registry.forget(session_id);
        } else {
            registry.close(session_id);
        }
    }

    /// Dispose of a topic we are no longer bound to. Paraysitic topics
    /// (zero activity — never saw any Telegram traffic) get deleted so the
    /// supergroup isn't littered with empty stubs from every interactive
    /// `claude --resume` session-picker. Topics that carry real history
    /// are only closed. Delete failures fall back to close.
    /// Returns `true` if the topic was deleted, `false` if closed.
    async fn retire_topic(&self, topic_id: i64, activity: u32) -> bool {
        if activity == 0 {
            match self
                .api
                .delete_forum_topic(&self.supergroup_id, topic_id)
                .await
            {
                Ok(()) => {
                    info!(topic_id, "deleted parasitic topic (zero activity)");
                    return true;
                }
                Err(e) => warn!(
                    error = %e,
                    topic_id,
                    "delete_forum_topic failed — falling back to close"
                ),
            }
        }
        if let Err(e) = self
            .api
            .close_forum_topic(&self.supergroup_id, topic_id)
            .await
        {
            warn!(error = %e, topic_id, "close_forum_topic failed");
        } else {
            info!(topic_id, activity, "closed retired topic");
        }
        false
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
            return;
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
