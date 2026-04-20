// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! `hdcd-telegram` — Rust drop-in replacement for the official Bun-based
//! Telegram channel plugin for Claude Code.
//!
//! Exposes the `telegram` module with all channel functionality:
//! access control, Bot API client, message handlers, polling, tools,
//! voice transcription, and shared types.

pub mod claude_session;
pub mod fs_perms;
pub mod router;
pub mod telegram;
pub mod token;
