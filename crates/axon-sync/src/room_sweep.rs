//! Shared bounded room-summary traversal for local SDK reconciliation.
use axon_store::Store;
use matrix_sdk::ruma::OwnedRoomId;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) struct RoomSweep {
    cursor: String,
    next: tokio::time::Instant,
    repeat: bool,
}

impl RoomSweep {
    pub(crate) fn new() -> Self {
        Self {
            cursor: String::new(),
            next: tokio::time::Instant::now(),
            repeat: false,
        }
    }

    /// Wake an idle traversal without resetting an active cursor: repeated
    /// lost hints must not starve rooms at the end of a large account.
    pub(crate) fn wake(&mut self) {
        self.repeat |= !self.cursor.is_empty();
        self.next = tokio::time::Instant::now();
    }

    fn advance(&mut self, page: &[String]) {
        if let Some(last) = page.last() {
            self.cursor.clone_from(last);
        } else {
            self.cursor.clear();
            self.next = tokio::time::Instant::now()
                + if self.repeat {
                    Duration::ZERO
                } else {
                    Duration::from_secs(300)
                };
            self.repeat = false;
        }
    }

    pub(crate) async fn page(
        &mut self,
        store: &Store,
        account_id: Uuid,
        cancel: &CancellationToken,
    ) -> Vec<OwnedRoomId> {
        if tokio::time::Instant::now() < self.next {
            return Vec::new();
        }
        let result = tokio::select! {
            _ = cancel.cancelled() => return Vec::new(),
            result = tokio::time::timeout(Duration::from_secs(2), store.state_reconciliation_rooms(account_id, &self.cursor)) => result,
        };
        match result {
            Ok(Ok(page)) => {
                self.advance(&page);
                page.into_iter().filter_map(|id| id.parse().ok()).collect()
            }
            _ => {
                tracing::warn!(%account_id, source = "sdk_cache", "local SDK reconciliation sweep page failed; will retry");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn lag_wakes_idle_sweep_and_never_rewinds_active_traversal() {
        let mut sweep = RoomSweep::new();
        sweep.advance(&["!a:localhost".into(), "!b:localhost".into()]);
        sweep.wake();
        assert_eq!(sweep.cursor, "!b:localhost");
        sweep.advance(&[]);
        assert!(sweep.cursor.is_empty());
        // A wake during traversal requests a fresh pass after the tail,
        // rather than being swallowed by the end-of-sweep idle transition.
        assert!(sweep.next <= tokio::time::Instant::now());
        sweep.advance(&[]);
        assert!(sweep.next > tokio::time::Instant::now());
        sweep.wake();
        assert!(sweep.next <= tokio::time::Instant::now());
    }
}
