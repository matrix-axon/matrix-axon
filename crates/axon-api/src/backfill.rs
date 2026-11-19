//! Backfill status port (M10).
//!
//! `GET /v1/status` reports the history-backfill engine's disk-space health so a
//! client can tell when backfill has paused (and why). Like the other engine-
//! backed surfaces, the API depends only on this port; the binary injects an
//! adapter over `axon_sync::BackfillHealth`, keeping this crate free of
//! `axon-sync`.

/// A point-in-time snapshot of the backfill engine's disk-space health.
#[derive(Debug, Clone, Copy)]
pub struct BackfillStatusSnapshot {
    /// Backfill is currently paused because free disk space is below the
    /// configured threshold. Live sync is unaffected.
    pub paused_low_disk: bool,
    /// Free bytes last observed on the guarded filesystem.
    pub free_bytes: u64,
    /// Events that gap pages inserted since the process started (ADR 0140).
    pub gap_events_recovered: u64,
}

/// Port exposing the backfill engine's disk-space health to `GET /v1/status`.
pub trait BackfillStatusProvider: Send + Sync {
    /// The current health snapshot (cheap; reads a shared atomic).
    fn snapshot(&self) -> BackfillStatusSnapshot;
}

/// Fallback provider used when the binary injects none (e.g. in API tests, or a
/// build with backfill compiled out): always reports a healthy, unpaused engine.
pub(crate) struct NoBackfillStatus;

impl BackfillStatusProvider for NoBackfillStatus {
    fn snapshot(&self) -> BackfillStatusSnapshot {
        BackfillStatusSnapshot {
            paused_low_disk: false,
            free_bytes: 0,
            gap_events_recovered: 0,
        }
    }
}
