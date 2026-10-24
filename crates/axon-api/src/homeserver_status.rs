//! Homeserver load-gate status port, for `GET /v1/status` (ADR 0122).
//!
//! Reports each homeserver's load gate — the adaptive limit on axon's own
//! background requests, the slots in use, and the circuit breaker — so an
//! operator can tell when background work (backfill, probes, key-backup
//! downloads) has stopped because a homeserver is struggling. Like
//! [`crate::sync_status`], the API depends only on this port; the binary
//! injects an adapter over `axon_sync::HomeserverGates`.

use std::time::{Duration, SystemTime};

/// A point-in-time snapshot of one homeserver's load gate.
#[derive(Debug, Clone)]
pub struct HomeserverGateSnapshot {
    /// The gate's key: the homeserver's base URL reduced to its origin.
    pub homeserver: String,
    pub limit: u32,
    pub active: u32,
    pub held: u32,
    pub waiting: u32,
    pub oldest_wait: Option<Duration>,
    pub breaker: HomeserverBreakerSnapshot,
}

/// The circuit breaker's state in a [`HomeserverGateSnapshot`].
#[derive(Debug, Clone, Copy)]
pub enum HomeserverBreakerSnapshot {
    Closed,
    Open {
        opened_at: SystemTime,
        next_probe_at: SystemTime,
    },
    Probing {
        opened_at: SystemTime,
    },
}

/// Port exposing every homeserver's load gate to `GET /v1/status`.
pub trait HomeserverStatusProvider: Send + Sync {
    /// A snapshot of every gate (cheap; in-memory, no database query).
    fn snapshot(&self) -> Vec<HomeserverGateSnapshot>;
}

/// Fallback provider used when the binary injects none (e.g. in API tests):
/// reports no homeservers.
pub(crate) struct NoHomeserverStatus;

impl HomeserverStatusProvider for NoHomeserverStatus {
    fn snapshot(&self) -> Vec<HomeserverGateSnapshot> {
        Vec::new()
    }
}
