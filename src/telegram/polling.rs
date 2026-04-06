// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Maciej Ostaszewski / HyperDev P.S.A.

//! Long-polling loop for `getUpdates` with 409-conflict retry.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::api::BotApi;
use super::types::Update;

/// Long-poll timeout in seconds (Telegram's maximum is 60).
const POLL_TIMEOUT: u64 = 30;

/// Run the `getUpdates` polling loop. Sends each `Update` on `tx`.
/// Returns when `cancel` token fires or an unrecoverable error occurs.
pub async fn run(
    api: Arc<BotApi>,
    tx: mpsc::Sender<Update>,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut offset: Option<i64> = None;
    let mut attempt: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            info!("polling loop: cancellation requested");
            return Ok(());
        }

        let result = tokio::select! {
            r = api.get_updates(offset, POLL_TIMEOUT) => r,
            _ = cancel.cancelled() => {
                info!("polling loop: cancelled during getUpdates");
                return Ok(());
            }
        };

        match result {
            Ok(resp) => {
                if resp.ok {
                    attempt = 0;
                    for update in &resp.result {
                        offset = Some(update.update_id + 1);
                        if tx.send(update.clone()).await.is_err() {
                            info!("polling loop: receiver dropped, exiting");
                            return Ok(());
                        }
                    }
                } else if resp.error_code == Some(409) {
                    attempt += 1;
                    let delay = std::cmp::min(1000 * (attempt as u64), 15000);
                    if attempt == 1 {
                        warn!("409 Conflict -- another instance is polling. Retrying in {delay}ms");
                    } else {
                        warn!("409 Conflict, retry #{attempt} in {delay}ms");
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {}
                        _ = cancel.cancelled() => return Ok(()),
                    }
                } else {
                    error!(
                        error_code = resp.error_code,
                        description = resp.description,
                        "getUpdates API error"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        _ = cancel.cancelled() => return Ok(()),
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "getUpdates network error");
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    _ = cancel.cancelled() => return Ok(()),
                }
            }
        }
    }
}
