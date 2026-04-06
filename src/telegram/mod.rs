// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Telegram channel module.
//!
//! Provides a standalone MCP server that bridges Telegram to Claude Code
//! via the `claude/channel` capability. Drop-in replacement for the
//! official Bun-based Telegram plugin, using ~5 MB RAM instead of ~100 MB.

pub mod access;
pub mod api;
pub mod handlers;
pub mod permission;
pub mod polling;
pub mod tools;
pub mod transcribe;
pub mod types;
