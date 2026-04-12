// SPDX-License-Identifier: Apache-2.0

//! Router module — multiplexes multiple Claude Code sessions through
//! forum topics in a single Telegram supergroup.

pub mod config;
pub mod mailbox;
pub mod sessions;
pub mod topics;
