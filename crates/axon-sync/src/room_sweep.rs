//! Shared bounded room-summary traversal for local SDK reconciliation.
use axon_store::Store;
use matrix_sdk::ruma::OwnedRoomId;
use std::{collections::HashSet, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) enum HintRead {
    Room(OwnedRoomId),
    Empty,
    Closed,
    Lagged,
}

pub(crate) struct HintBatch {
    pub(crate) rooms: HashSet<OwnedRoomId>,
    pub(crate) closed: bool,
    pub(crate) lagged: bool,
}

/// Both local-reconciliation workers use the same bounded receive policy.
/// Stop at four distinct rooms or 32 receives, preserving the channel tail.
pub(crate) fn take_hints(mut receive: impl FnMut() -> HintRead) -> HintBatch {
    let mut batch = HintBatch {
        rooms: HashSet::new(),
        closed: false,
        lagged: false,
    };
    for _ in 0..32 {
        if batch.rooms.len() == 4 {
            break;
        }
        match receive() {
            HintRead::Room(room) => {
                batch.rooms.insert(room);
            }
            HintRead::Empty => break,
            HintRead::Closed => {
                batch.closed = true;
                break;
            }
            HintRead::Lagged => {
                batch.lagged = true;
                break;
            }
        }
    }
    batch
}

pub(crate) fn interval() -> tokio::time::Interval {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

pub(crate) struct RoomSweep {
    cursor: String,
    next: tokio::time::Instant,
    repeat: bool,
    started: Option<tokio::time::Instant>,
    visited: u64,
}

impl RoomSweep {
    pub(crate) fn new() -> Self {
        Self {
            cursor: String::new(),
            next: tokio::time::Instant::now(),
            repeat: false,
            started: None,
            visited: 0,
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

    pub(crate) fn is_idle(&self) -> bool {
        self.cursor.is_empty() && tokio::time::Instant::now() < self.next
    }

    pub(crate) async fn page(
        &mut self,
        store: &Store,
        account_id: Uuid,
        cancel: &CancellationToken,
        worker: &'static str,
    ) -> Vec<OwnedRoomId> {
        if self.is_idle() {
            return Vec::new();
        }
        if self.started.is_none() {
            self.started = Some(tokio::time::Instant::now());
            self.visited = 0;
            tracing::debug!(%account_id, worker, source = "sdk_cache", "local SDK reconciliation sweep started");
        }
        let result = tokio::select! {
            _ = cancel.cancelled() => return Vec::new(),
            result = tokio::time::timeout(Duration::from_secs(2), store.state_reconciliation_rooms(account_id, &self.cursor)) => result,
        };
        match result {
            Ok(Ok(page)) => {
                self.visited = self.visited.saturating_add(page.len() as u64);
                if page.is_empty() {
                    let elapsed_seconds = self
                        .started
                        .take()
                        .map_or(0.0, |start| start.elapsed().as_secs_f64());
                    tracing::debug!(%account_id, worker, source = "sdk_cache", rooms_visited = self.visited, elapsed_seconds, repeat_requested = self.repeat, "local SDK reconciliation sweep completed");
                }
                self.advance(&page);
                page.into_iter().filter_map(|id| id.parse().ok()).collect()
            }
            Ok(Err(error)) => {
                tracing::warn!(%account_id, worker, source = "sdk_cache", reason = error.diagnostic_reason(), "local SDK reconciliation sweep page failed; will retry");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(%account_id, worker, source = "sdk_cache", reason = "deadline", "local SDK reconciliation sweep page timed out; will retry");
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
