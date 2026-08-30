//! Shared bounded-lifetime machinery for Matrix OAuth QR flows.
//!
//! Acquisition and grant flows have different wire state and protocol drivers,
//! but their capacity lease, finalization fence, terminal retention, and reaper
//! timing are one lifecycle contract. Keep that contract here so a race fix or
//! limit change cannot land in only one flow family.

use std::time::{Duration, Instant};

use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

pub(crate) const MAX_CONCURRENT_FLOWS: usize = 8;
pub(crate) const MAX_RETAINED_FLOWS: usize = 256;
pub(crate) const FLOW_TTL: Duration = Duration::from_secs(10 * 60);
pub(crate) const TERMINAL_RETENTION: Duration = Duration::from_secs(2 * 60);
const REAPER_INTERVAL: Duration = Duration::from_secs(5);

/// Exactly-once owner of one active-flow capacity permit.
///
/// `finalizing` is used by account acquisition after the remote protocol has
/// succeeded: cancellation must stop competing once crash-safe adoption owns
/// the result, while the permit remains live until adoption reaches a terminal
/// state. Grant flows never claim finalization and use the same lease as a
/// straight terminal-transition fence.
pub(crate) struct FlowLease {
    permit: Option<OwnedSemaphorePermit>,
    terminal_at: Option<Instant>,
    finalizing: bool,
}

impl FlowLease {
    pub(crate) fn new(permit: OwnedSemaphorePermit) -> Self {
        Self {
            permit: Some(permit),
            terminal_at: None,
            finalizing: false,
        }
    }

    pub(crate) fn is_live(&self) -> bool {
        self.permit.is_some()
    }

    pub(crate) fn is_finalizing(&self) -> bool {
        self.finalizing
    }

    pub(crate) fn claim_finalization(&mut self) -> bool {
        if !self.is_live() || self.finalizing {
            return false;
        }
        self.finalizing = true;
        true
    }

    pub(crate) fn finish(&mut self, now: Instant) -> bool {
        if self.permit.take().is_none() {
            return false;
        }
        self.terminal_at = Some(now);
        true
    }

    pub(crate) fn cancel(&mut self, now: Instant) -> bool {
        if self.finalizing {
            return false;
        }
        self.finish(now)
    }

    pub(crate) fn is_retained_at(&self, now: Instant) -> bool {
        self.terminal_at
            .is_none_or(|at| now.duration_since(at) < TERMINAL_RETENTION)
    }
}

pub(crate) async fn run_reaper(cancel: CancellationToken, mut reap: impl FnMut()) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(REAPER_INTERVAL) => reap(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Semaphore;

    use super::*;

    fn lease() -> (FlowLease, Arc<Semaphore>) {
        let capacity = Arc::new(Semaphore::new(1));
        let permit = capacity.clone().try_acquire_owned().unwrap();
        (FlowLease::new(permit), capacity)
    }

    #[test]
    fn finish_releases_capacity_once_and_retains_the_snapshot_briefly() {
        let (mut lease, capacity) = lease();
        let now = Instant::now();

        assert!(lease.finish(now));
        assert!(!lease.finish(now));
        assert_eq!(capacity.available_permits(), 1);
        assert!(lease.is_retained_at(now + TERMINAL_RETENTION - Duration::from_millis(1)));
        assert!(!lease.is_retained_at(now + TERMINAL_RETENTION));
    }

    #[test]
    fn finalization_fences_cancellation_but_still_allows_completion() {
        let (mut lease, capacity) = lease();
        let now = Instant::now();

        assert!(lease.claim_finalization());
        assert!(!lease.claim_finalization());
        assert!(!lease.cancel(now));
        assert_eq!(capacity.available_permits(), 0);
        assert!(lease.finish(now));
        assert_eq!(capacity.available_permits(), 1);
    }
}
