//! Paced, resumable upgrade repair outside the startup migration transaction.

use std::time::Duration;

use axon_store::Store;

/// One maintenance worker per process. Every batch commits independently and
/// NULL projections remain pending across cancellation, failure, or shutdown.
/// Like the upload sweeper, this has no in-memory state to drain at shutdown.
pub(crate) fn spawn_backfill(store: Store) {
    tokio::spawn(async move {
        loop {
            match store.backfill_inline_media_batch().await {
                Ok(0) => {
                    tracing::debug!("inline media historical backfill complete");
                    return;
                }
                Ok(rows) => tracing::debug!(rows, "backfilled inline media references"),
                Err(error) => tracing::warn!(
                    reason = error.diagnostic_reason(),
                    "inline media backfill failed; will retry"
                ),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}
