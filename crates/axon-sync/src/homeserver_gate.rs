//! A load gate between axon's background work and each homeserver (ADR 0122).
//!
//! Every background request takes a [`Permit`] from its homeserver's
//! [`HomeserverGate`] before it is sent, and reports how it ended. The gate
//! admits requests while the slots in use (active permits plus held slots) are
//! fewer than an adaptive limit:
//!
//! - fast successes grow the limit by one, slowly, up to [`MAX_LIMIT`];
//! - a timeout, 5xx, 429 or transport error halves it, once per burst;
//! - at zero a circuit breaker opens, and one probe goes out after a cooldown
//!   once no held slots remain;
//! - a timed-out request keeps its slot for [`TIMEOUT_HOLD`], because the
//!   homeserver is presumably still working on it.
//!
//! Three other kinds of caller see the gate without taking a slot:
//!
//! - **Interactive** requests (a user is waiting) never wait. Their timeouts and
//!   server errors can halve the limit, but no lower than 1.
//! - **Long-polls** (the ADR 0124 image-pack transport) wait only while the
//!   breaker is not closed, and hold no slot while polling, since a poll is
//!   meant to sit at the homeserver for its whole server-side timeout. Their
//!   latency is never classified. Hard failures halve the limit like any
//!   background request, and an outer timeout leaves a held slot, because the
//!   homeserver may still be working on that abandoned poll. When no background
//!   request is waiting, a long-poll is the breaker's probe, so a gate whose
//!   only traffic is a long-poll can still close again.
//! - The media proxy reports nothing: a slow remote media server says nothing
//!   about the homeserver's load.
//!
//! Waiting for a permit is not part of a request: callers start their own
//! timeout once [`HomeserverGate::acquire`] returns, and a closed gate is never
//! recorded as a failure. Dropping a waiting `acquire` (cancellation) leaves the
//! queue; dropping a [`Permit`] without [`Permit::finish`] frees its slot and
//! teaches the gate nothing.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use matrix_sdk::{Client, HttpError};
use tokio::sync::Notify;
use tokio::time::error::Elapsed;
use tokio::time::Instant;
use url::Url;

/// The limit a new gate starts at. Backfill pages one room at a time per
/// account; 2 lets another caller run beside it.
pub(crate) const INITIAL_LIMIT: u32 = 2;
/// The ceiling on the limit. Background work is never urgent.
pub(crate) const MAX_LIMIT: u32 = 4;
/// Fast successes since the limit last changed before it grows by one.
pub(crate) const GROW_AFTER: u32 = 10;
/// How long a timed-out request keeps its slot. Outlasts 43 % of the
/// 2026-10-17 incident's post-timeout work (ADR 0122 decision 4).
pub(crate) const TIMEOUT_HOLD: Duration = Duration::from_secs(15 * 60);
/// The breaker's first open period. Matters only when it opened on 5xx, 429 or
/// transport errors: after a timeout the held slot outlasts it.
pub(crate) const COOLDOWN: Duration = Duration::from_secs(60);
/// The cap on the doubling open period.
pub(crate) const COOLDOWN_MAX: Duration = Duration::from_secs(30 * 60);

/// A success counts toward growth when it took under this fraction of the
/// request's own timeout (`FAST_FRACTION` = ½ in ADR 0122 decision 5).
fn is_fast(elapsed: Duration, timeout: Duration) -> bool {
    elapsed < timeout / 2
}

/// The open period after the breaker has reopened from probing `reopenings`
/// times since it last closed.
fn cooldown(reopenings: u32) -> Duration {
    COOLDOWN
        .saturating_mul(1 << reopenings.min(16))
        .min(COOLDOWN_MAX)
}

/// Whether a user is waiting on a request (ADR 0122 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Caller {
    /// Waits for a permit.
    Background,
    /// Never waits; its failures can only halve the limit, no lower than 1.
    Interactive,
}

/// How a request ended, as the gate sees it (ADR 0122 decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The homeserver answered successfully.
    Success,
    /// Axon stopped waiting. The homeserver is presumably still working on it.
    TimedOut,
    /// A 5xx, a 429 or a transport error.
    Overloaded,
    /// Any other 4xx, or an answer axon could not read: the request's problem,
    /// not load's.
    Refused,
    /// The request failed before it reached the homeserver (a local store or
    /// crypto error, say). Teaches the gate nothing, like a dropped permit.
    Local,
}

/// An error a gated request can end in.
pub(crate) trait GateError {
    fn outcome(&self) -> Outcome;
}

impl GateError for HttpError {
    fn outcome(&self) -> Outcome {
        match self {
            HttpError::Reqwest(error) if error.is_timeout() => Outcome::TimedOut,
            HttpError::Reqwest(_) => Outcome::Overloaded,
            // Ruma keeps the status of a non-JSON body too, so a proxy's HTML 502
            // is a 5xx here.
            HttpError::Api(_) => match self
                .as_client_api_error()
                .map(|error| error.status_code.as_u16())
            {
                Some(429 | 500..=599) => Outcome::Overloaded,
                _ => Outcome::Refused,
            },
            HttpError::Cached(inner) => inner.outcome(),
            _ => Outcome::Local,
        }
    }
}

impl GateError for matrix_sdk::Error {
    fn outcome(&self) -> Outcome {
        match self {
            matrix_sdk::Error::Http(error) => error.outcome(),
            _ => Outcome::Local,
        }
    }
}

/// The outcome of a request bounded by an outer `tokio::time::timeout`.
pub(crate) fn outcome_of<T, E: GateError>(result: &Result<Result<T, E>, Elapsed>) -> Outcome {
    match result {
        Err(_) => Outcome::TimedOut,
        Ok(Ok(_)) => Outcome::Success,
        Ok(Err(error)) => error.outcome(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Breaker {
    /// The limit is at least 1 and decision 2 applies.
    Closed,
    /// Nothing is admitted until `until` has passed and no held slots remain.
    /// `opened_at` is when the breaker left `Closed`.
    Open { opened_at: Instant, until: Instant },
    /// One probe is in flight.
    Probing { opened_at: Instant },
}

#[derive(Debug)]
struct State {
    limit: u32,
    /// Permits handed out and not yet finished or dropped.
    active: u32,
    /// When each held slot is released.
    held: Vec<Instant>,
    /// Fast successes since the limit last changed.
    fast: u32,
    /// Bumped on every decrease. A failure halves the limit only if its request
    /// started in the current epoch, so one burst counts once.
    epoch: u64,
    breaker: Breaker,
    /// Times the breaker has reopened from probing since it last closed.
    reopenings: u32,
    /// Background requests waiting for a permit, first in line first.
    queue: VecDeque<(u64, Instant)>,
    /// Long-polls waiting for the breaker to close, for `/v1/status` only.
    long_polls: HashMap<u64, Instant>,
    next_id: u64,
}

/// What a request may do once it has looked at the gate.
enum Admission {
    /// Send now, as an ordinary request (`probe: false`) or the breaker's probe.
    Admit { epoch: u64, probe: bool },
    /// Wait for the gate to change, or until this instant at the latest.
    Wait(Option<Instant>),
}

impl State {
    fn new() -> Self {
        State {
            limit: INITIAL_LIMIT,
            active: 0,
            held: Vec::new(),
            fast: 0,
            epoch: 0,
            breaker: Breaker::Closed,
            reopenings: 0,
            queue: VecDeque::new(),
            long_polls: HashMap::new(),
            next_id: 0,
        }
    }

    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn release_expired_holds(&mut self, now: Instant) {
        self.held.retain(|&until| until > now);
    }

    fn in_use(&self) -> u32 {
        self.active + self.held.len() as u32
    }

    fn earliest_hold(&self) -> Option<Instant> {
        self.held.iter().min().copied()
    }

    /// Whether a background request may be sent now, assuming it is first in
    /// line. Admitting it is the caller's job.
    fn admission(&mut self, now: Instant) -> Admission {
        self.release_expired_holds(now);
        match self.breaker {
            Breaker::Closed if self.in_use() < self.limit => Admission::Admit {
                epoch: self.epoch,
                probe: false,
            },
            Breaker::Closed => Admission::Wait(self.earliest_hold()),
            Breaker::Open { opened_at, until } if now >= until && self.held.is_empty() => {
                self.breaker = Breaker::Probing { opened_at };
                Admission::Admit {
                    epoch: self.epoch,
                    probe: true,
                }
            }
            Breaker::Open { until, .. } if now < until => Admission::Wait(Some(until)),
            Breaker::Open { .. } => Admission::Wait(self.earliest_hold()),
            Breaker::Probing { .. } => Admission::Wait(None),
        }
    }

    /// Halve the limit for a hard failure from a request that started in
    /// `epoch`, no lower than `floor`. Opens the breaker at zero.
    fn halve(&mut self, key: &str, now: Instant, epoch: u64, floor: u32, outcome: Outcome) {
        if epoch != self.epoch || self.breaker != Breaker::Closed {
            return;
        }
        let limit = (self.limit / 2).max(floor);
        if limit == self.limit {
            return;
        }
        self.limit = limit;
        self.epoch += 1;
        self.fast = 0;
        if limit == 0 {
            self.reopenings = 0;
            self.breaker = Breaker::Open {
                opened_at: now,
                until: now + cooldown(0),
            };
            tracing::warn!(
                homeserver = key,
                ?outcome,
                "homeserver struggling; background requests stopped"
            );
        } else {
            tracing::info!(
                homeserver = key,
                limit,
                ?outcome,
                "homeserver gate limit halved"
            );
        }
    }

    /// Apply the outcome of the breaker's probe (ADR 0122 decision 3).
    fn finish_probe(&mut self, key: &str, now: Instant, outcome: Outcome) {
        let Breaker::Probing { opened_at } = self.breaker else {
            return;
        };
        match outcome {
            Outcome::Success | Outcome::Refused => {
                self.breaker = Breaker::Closed;
                self.limit = 1;
                self.fast = 0;
                self.reopenings = 0;
                tracing::info!(
                    homeserver = key,
                    ?outcome,
                    "homeserver answered; background requests resume"
                );
            }
            Outcome::TimedOut | Outcome::Overloaded => {
                self.reopenings += 1;
                let open_for = cooldown(self.reopenings);
                self.breaker = Breaker::Open {
                    opened_at,
                    until: now + open_for,
                };
                tracing::info!(
                    homeserver = key,
                    ?outcome,
                    cooldown_secs = open_for.as_secs(),
                    "homeserver probe failed; background requests stay stopped"
                );
            }
            Outcome::Local => self.drop_probe(now),
        }
    }

    /// A probe that ended without telling the gate anything: the next waiter
    /// probes instead.
    fn drop_probe(&mut self, now: Instant) {
        if let Breaker::Probing { opened_at } = self.breaker {
            self.breaker = Breaker::Open {
                opened_at,
                until: now,
            };
        }
    }
}

/// The gate for one homeserver, shared by every account in the process that
/// uses it.
#[derive(Debug)]
pub(crate) struct HomeserverGate {
    key: String,
    state: Mutex<State>,
    changed: Notify,
}

impl HomeserverGate {
    fn new(key: String) -> Self {
        HomeserverGate {
            key,
            state: Mutex::new(State::new()),
            changed: Notify::new(),
        }
    }

    /// A gate outside any registry, for tests.
    #[cfg(test)]
    pub(crate) fn for_test() -> Arc<Self> {
        Arc::new(Self::new("http://hs.test:80".to_owned()))
    }

    /// Open the breaker as if the limit had just reached zero, for tests of the
    /// callers.
    #[cfg(test)]
    pub(crate) fn open_for_test(&self) {
        let now = Instant::now();
        let mut state = self.lock();
        state.limit = 0;
        state.epoch += 1;
        state.breaker = Breaker::Open {
            opened_at: now,
            until: now + COOLDOWN,
        };
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("homeserver gate state poisoned")
    }

    /// Wake every waiter to look at the gate again.
    fn wake(&self) {
        self.changed.notify_waiters();
    }

    /// Wait for a permit to send one background request bounded by `timeout`.
    /// FIFO. Cancel by dropping the future, which leaves the queue.
    pub(crate) async fn acquire(self: &Arc<Self>, timeout: Duration) -> Permit {
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            state.queue.push_back((id, Instant::now()));
            id
        };
        let mut queued = Queued {
            gate: self,
            id,
            admitted: false,
        };
        loop {
            // Registered before looking, so a change between the look and the
            // wait still wakes this waiter.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let admission = {
                let mut state = self.lock();
                if state.queue.front().map(|&(head, _)| head) == Some(id) {
                    let admission = state.admission(Instant::now());
                    if matches!(admission, Admission::Admit { .. }) {
                        state.queue.pop_front();
                        state.active += 1;
                    }
                    admission
                } else {
                    Admission::Wait(None)
                }
            };
            match admission {
                Admission::Admit { epoch, probe } => {
                    queued.admitted = true;
                    // The next in line may be admissible too.
                    self.wake();
                    return Permit::new(self, timeout, epoch, probe);
                }
                Admission::Wait(Some(deadline)) => {
                    tokio::select! {
                        _ = changed => {}
                        _ = tokio::time::sleep_until(deadline) => {}
                    }
                }
                Admission::Wait(None) => changed.await,
            }
        }
    }

    /// A permit if one can be had without waiting and nobody is queued, for a
    /// caller that must not wait (it runs inside a loop that has other work).
    pub(crate) fn try_acquire(self: &Arc<Self>, timeout: Duration) -> Option<Permit> {
        let mut state = self.lock();
        if !state.queue.is_empty() {
            return None;
        }
        match state.admission(Instant::now()) {
            Admission::Admit { epoch, probe } => {
                state.active += 1;
                drop(state);
                Some(Permit::new(self, timeout, epoch, probe))
            }
            Admission::Wait(_) => None,
        }
    }

    /// One background request: wait for a permit, run `request` bounded by
    /// `timeout`, and report how it ended. For requests sent through an SDK
    /// method, the outer timeout also bounds the SDK's own retries.
    pub(crate) async fn call<T, E: GateError>(
        self: &Arc<Self>,
        timeout: Duration,
        request: impl Future<Output = Result<T, E>>,
    ) -> Result<Result<T, E>, Elapsed> {
        let permit = self.acquire(timeout).await;
        let result = tokio::time::timeout(timeout, request).await;
        permit.finish(outcome_of(&result));
        result
    }

    /// One interactive request: never waits; a timeout or server error can
    /// halve the limit, no lower than 1, and nothing else changes.
    pub(crate) async fn call_interactive<T, E: GateError>(
        self: &Arc<Self>,
        timeout: Duration,
        request: impl Future<Output = Result<T, E>>,
    ) -> Result<Result<T, E>, Elapsed> {
        let epoch = self.epoch();
        let result = tokio::time::timeout(timeout, request).await;
        self.report_interactive(epoch, outcome_of(&result));
        result
    }

    /// [`call`](Self::call) or [`call_interactive`](Self::call_interactive), by
    /// `caller`.
    pub(crate) async fn call_as<T, E: GateError>(
        self: &Arc<Self>,
        caller: Caller,
        timeout: Duration,
        request: impl Future<Output = Result<T, E>>,
    ) -> Result<Result<T, E>, Elapsed> {
        match caller {
            Caller::Background => self.call(timeout, request).await,
            Caller::Interactive => self.call_interactive(timeout, request).await,
        }
    }

    /// Feed an interactive request's outcome to the gate. `epoch` is
    /// [`epoch`](Self::epoch) as it was when the request started.
    pub(crate) fn report_interactive(&self, epoch: u64, outcome: Outcome) {
        if matches!(outcome, Outcome::TimedOut | Outcome::Overloaded) {
            let mut state = self.lock();
            state.halve(&self.key, Instant::now(), epoch, 1, outcome);
        }
    }

    /// The current decrease epoch, read when an interactive request starts.
    pub(crate) fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Wait until a long-poll may be sent: while the breaker is closed, at once.
    /// Cancel by dropping the future.
    pub(crate) async fn long_poll(self: &Arc<Self>) -> LongPoll {
        let id = {
            let mut state = self.lock();
            let id = state.next_id();
            state.long_polls.insert(id, Instant::now());
            id
        };
        let _waiting = LongPollWaiting { gate: self, id };
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let admission = {
                let mut state = self.lock();
                let now = Instant::now();
                state.release_expired_holds(now);
                let breaker = state.breaker;
                match breaker {
                    Breaker::Closed => Admission::Admit {
                        epoch: state.epoch,
                        probe: false,
                    },
                    // Background requests probe first; a long-poll probes only
                    // when none is waiting.
                    Breaker::Open { .. } if state.queue.is_empty() => state.admission(now),
                    Breaker::Open { until, .. } if now < until => Admission::Wait(Some(until)),
                    Breaker::Open { .. } => Admission::Wait(state.earliest_hold()),
                    Breaker::Probing { .. } => Admission::Wait(None),
                }
            };
            match admission {
                Admission::Admit { epoch, probe } => {
                    return LongPoll {
                        gate: Arc::clone(self),
                        epoch,
                        probe,
                        reported: false,
                    }
                }
                Admission::Wait(Some(deadline)) => {
                    tokio::select! {
                        _ = changed => {}
                        _ = tokio::time::sleep_until(deadline) => {}
                    }
                }
                Admission::Wait(None) => changed.await,
            }
        }
    }

    fn finish(&self, permit: &Permit, outcome: Outcome) {
        let now = Instant::now();
        let mut state = self.lock();
        state.active -= 1;
        if outcome == Outcome::TimedOut {
            state.held.push(now + TIMEOUT_HOLD);
        }
        if permit.probe {
            state.finish_probe(&self.key, now, outcome);
        } else if state.breaker == Breaker::Closed {
            match outcome {
                Outcome::Success if is_fast(now - permit.started, permit.timeout) => {
                    state.fast += 1;
                    if state.fast >= GROW_AFTER {
                        state.fast = 0;
                        if state.limit < MAX_LIMIT {
                            state.limit += 1;
                            tracing::debug!(homeserver = %self.key, limit = state.limit, "homeserver gate limit grew");
                        }
                    }
                }
                Outcome::TimedOut | Outcome::Overloaded => {
                    state.halve(&self.key, now, permit.epoch, 0, outcome);
                }
                Outcome::Success | Outcome::Refused | Outcome::Local => {}
            }
        }
        drop(state);
        self.wake();
    }

    fn release(&self, probe: bool) {
        let mut state = self.lock();
        state.active -= 1;
        if probe {
            state.drop_probe(Instant::now());
        }
        drop(state);
        self.wake();
    }

    fn finish_long_poll(&self, poll: &LongPoll, outcome: Outcome) {
        let now = Instant::now();
        let mut state = self.lock();
        if outcome == Outcome::TimedOut {
            state.held.push(now + TIMEOUT_HOLD);
        }
        if poll.probe {
            state.finish_probe(&self.key, now, outcome);
        } else if matches!(outcome, Outcome::TimedOut | Outcome::Overloaded) {
            state.halve(&self.key, now, poll.epoch, 0, outcome);
        }
        drop(state);
        self.wake();
    }

    pub(crate) fn snapshot(&self) -> GateSnapshot {
        let now = Instant::now();
        let wall = SystemTime::now();
        let at = |instant: Instant| {
            if instant >= now {
                wall + (instant - now)
            } else {
                wall - (now - instant)
            }
        };
        let mut state = self.lock();
        state.release_expired_holds(now);
        let oldest = state
            .queue
            .iter()
            .map(|&(_, since)| since)
            .chain(state.long_polls.values().copied())
            .min();
        GateSnapshot {
            homeserver: self.key.clone(),
            limit: state.limit,
            active: state.active,
            held: state.held.len() as u32,
            waiting: (state.queue.len() + state.long_polls.len()) as u32,
            oldest_wait: oldest.map(|since| now - since),
            breaker: match state.breaker {
                Breaker::Closed => BreakerSnapshot::Closed,
                Breaker::Open { opened_at, until } => BreakerSnapshot::Open {
                    opened_at: at(opened_at),
                    // A probe also waits for every held slot to be released.
                    next_probe_at: at(state.held.iter().copied().fold(until, Instant::max)),
                },
                Breaker::Probing { opened_at } => BreakerSnapshot::Probing {
                    opened_at: at(opened_at),
                },
            },
        }
    }
}

/// Takes a waiting `acquire` out of the queue if it is dropped before it is
/// admitted.
struct Queued<'a> {
    gate: &'a HomeserverGate,
    id: u64,
    admitted: bool,
}

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        if self.admitted {
            return;
        }
        let mut state = self.gate.lock();
        state.queue.retain(|&(id, _)| id != self.id);
        drop(state);
        // The waiter behind this one may now be first in line.
        self.gate.wake();
    }
}

struct LongPollWaiting<'a> {
    gate: &'a HomeserverGate,
    id: u64,
}

impl Drop for LongPollWaiting<'_> {
    fn drop(&mut self) {
        self.gate.lock().long_polls.remove(&self.id);
    }
}

/// A slot for one background request. Report how it ended with
/// [`finish`](Self::finish); dropping it instead frees the slot and teaches the
/// gate nothing.
#[must_use = "a permit reports its request's outcome with `finish`"]
#[derive(Debug)]
pub(crate) struct Permit {
    gate: Arc<HomeserverGate>,
    started: Instant,
    timeout: Duration,
    epoch: u64,
    probe: bool,
    finished: bool,
}

impl Permit {
    fn new(gate: &Arc<HomeserverGate>, timeout: Duration, epoch: u64, probe: bool) -> Self {
        Permit {
            gate: Arc::clone(gate),
            started: Instant::now(),
            timeout,
            epoch,
            probe,
            finished: false,
        }
    }

    /// Report how the request ended. A timeout keeps the slot for
    /// [`TIMEOUT_HOLD`].
    pub(crate) fn finish(mut self, outcome: Outcome) {
        self.finished = true;
        if outcome == Outcome::Local {
            self.gate.release(self.probe);
        } else {
            self.gate.finish(&self, outcome);
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.finished {
            self.gate.release(self.probe);
        }
    }
}

/// Clearance for one long-poll. Report how it ended with
/// [`finish`](Self::finish).
#[must_use = "a long-poll reports its outcome with `finish`"]
#[derive(Debug)]
pub(crate) struct LongPoll {
    gate: Arc<HomeserverGate>,
    epoch: u64,
    probe: bool,
    reported: bool,
}

impl LongPoll {
    /// Report how the poll ended. Its latency is never classified, so a
    /// success changes nothing unless the poll was the breaker's probe.
    pub(crate) fn finish(mut self, outcome: Outcome) {
        self.reported = true;
        if outcome == Outcome::Local {
            if self.probe {
                self.gate.release_long_poll_probe();
            }
        } else {
            self.gate.finish_long_poll(&self, outcome);
        }
    }
}

impl HomeserverGate {
    fn release_long_poll_probe(&self) {
        self.lock().drop_probe(Instant::now());
        self.wake();
    }
}

impl Drop for LongPoll {
    fn drop(&mut self) {
        if !self.reported && self.probe {
            self.gate.release_long_poll_probe();
        }
    }
}

/// One gate's state for `GET /v1/status` (ADR 0122 decision 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateSnapshot {
    /// The gate's key: the homeserver's base URL reduced to its origin.
    pub homeserver: String,
    pub limit: u32,
    /// Background requests in flight.
    pub active: u32,
    /// Slots held by timed-out requests the homeserver may still be working on.
    pub held: u32,
    /// Requests waiting for the gate, background and long-poll.
    pub waiting: u32,
    /// How long the longest-waiting request has waited.
    pub oldest_wait: Option<Duration>,
    pub breaker: BreakerSnapshot,
}

/// The breaker's state in a [`GateSnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerSnapshot {
    Closed,
    /// `opened_at` is when the breaker left closed; `next_probe_at` is the
    /// earliest a probe may go out (the cooldown, and every held slot, over).
    Open {
        opened_at: SystemTime,
        next_probe_at: SystemTime,
    },
    Probing {
        opened_at: SystemTime,
    },
}

/// Every homeserver's gate, keyed by origin. Cheap to clone; clones share the
/// gates.
#[derive(Debug, Clone, Default)]
pub struct HomeserverGates {
    gates: Arc<Mutex<HashMap<String, Arc<HomeserverGate>>>>,
}

impl HomeserverGates {
    /// The gate for the homeserver `client` settled on after discovery.
    pub(crate) fn for_client(&self, client: &Client) -> Arc<HomeserverGate> {
        self.for_url(&client.homeserver())
    }

    fn for_url(&self, url: &Url) -> Arc<HomeserverGate> {
        let key = gate_key(url);
        let mut gates = self.gates.lock().expect("homeserver gates poisoned");
        Arc::clone(
            gates
                .entry(key.clone())
                .or_insert_with(|| Arc::new(HomeserverGate::new(key))),
        )
    }

    /// Every gate's state, ordered by homeserver.
    pub fn snapshot(&self) -> Vec<GateSnapshot> {
        let gates: Vec<_> = self
            .gates
            .lock()
            .expect("homeserver gates poisoned")
            .values()
            .cloned()
            .collect();
        let mut snapshots: Vec<_> = gates.iter().map(|gate| gate.snapshot()).collect();
        snapshots.sort_by(|a, b| a.homeserver.cmp(&b.homeserver));
        snapshots
    }
}

/// A base URL reduced to its origin, with the port explicit. `Url` already
/// lower-cases the scheme and host.
fn gate_key(url: &Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(30);

    fn state(gate: &HomeserverGate) -> (u32, u32, u32, Breaker) {
        let mut state = gate.lock();
        state.release_expired_holds(Instant::now());
        (
            state.limit,
            state.active,
            state.held.len() as u32,
            state.breaker,
        )
    }

    fn is_open(gate: &HomeserverGate) -> bool {
        matches!(state(gate).3, Breaker::Open { .. })
    }

    /// Drive the gate to an open breaker: two concurrent overloads halve 2 → 1
    /// once, and a third, started after that decrease, opens it.
    async fn open_on_overload(gate: &Arc<HomeserverGate>) {
        let a = gate.acquire(T).await;
        let b = gate.acquire(T).await;
        a.finish(Outcome::Overloaded);
        b.finish(Outcome::Overloaded);
        assert_eq!(state(gate).0, 1);
        gate.acquire(T).await.finish(Outcome::Overloaded);
        assert!(is_open(gate));
    }

    #[tokio::test(start_paused = true)]
    async fn the_limit_grows_by_one_after_ten_fast_successes_up_to_the_ceiling() {
        let gate = HomeserverGate::for_test();
        for _ in 0..GROW_AFTER - 1 {
            gate.acquire(T).await.finish(Outcome::Success);
        }
        assert_eq!(state(&gate).0, INITIAL_LIMIT);
        gate.acquire(T).await.finish(Outcome::Success);
        assert_eq!(state(&gate).0, INITIAL_LIMIT + 1);
        for _ in 0..GROW_AFTER * 10 {
            gate.acquire(T).await.finish(Outcome::Success);
        }
        assert_eq!(state(&gate).0, MAX_LIMIT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_success_or_another_4xx_is_neutral() {
        let gate = HomeserverGate::for_test();
        for _ in 0..GROW_AFTER * 2 {
            let permit = gate.acquire(T).await;
            tokio::time::advance(T / 2).await;
            permit.finish(Outcome::Success);
            gate.acquire(T).await.finish(Outcome::Refused);
        }
        assert_eq!(state(&gate), (INITIAL_LIMIT, 0, 0, Breaker::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_halves_the_limit_once() {
        let gate = HomeserverGate::for_test();
        for _ in 0..GROW_AFTER * 2 {
            gate.acquire(T).await.finish(Outcome::Success);
        }
        assert_eq!(state(&gate).0, 4);
        let permits = [
            gate.acquire(T).await,
            gate.acquire(T).await,
            gate.acquire(T).await,
            gate.acquire(T).await,
        ];
        for permit in permits {
            permit.finish(Outcome::Overloaded);
        }
        assert_eq!(state(&gate).0, 2, "four failures from one burst halve once");
        gate.acquire(T).await.finish(Outcome::Overloaded);
        assert_eq!(
            state(&gate).0,
            1,
            "a request started after the decrease halves again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_admitted_while_slots_in_use_exceed_a_lowered_limit() {
        let gate = HomeserverGate::for_test();
        for _ in 0..GROW_AFTER * 2 {
            gate.acquire(T).await.finish(Outcome::Success);
        }
        let mut permits = vec![
            gate.acquire(T).await,
            gate.acquire(T).await,
            gate.acquire(T).await,
            gate.acquire(T).await,
        ];
        permits.pop().unwrap().finish(Outcome::Overloaded);
        assert_eq!(state(&gate), (2, 3, 0, Breaker::Closed));
        assert!(gate.try_acquire(T).is_none(), "3 in use at a limit of 2");
        permits.pop().unwrap().finish(Outcome::Success);
        assert!(gate.try_acquire(T).is_none(), "2 in use at a limit of 2");
        permits.pop().unwrap().finish(Outcome::Success);
        assert!(gate.try_acquire(T).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_request_keeps_its_slot_for_the_hold() {
        let gate = HomeserverGate::for_test();
        let a = gate.acquire(T).await;
        let b = gate.acquire(T).await;
        a.finish(Outcome::TimedOut);
        b.finish(Outcome::TimedOut);
        // The incident's shape: both held at a limit of 1, the breaker closed.
        assert_eq!(state(&gate), (1, 0, 2, Breaker::Closed));

        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                let permit = gate.acquire(T).await;
                let admitted = Instant::now();
                permit.finish(Outcome::Success);
                admitted
            }
        });
        let start = Instant::now();
        let admitted = waiter.await.unwrap();
        assert_eq!(admitted - start, TIMEOUT_HOLD);
    }

    #[tokio::test(start_paused = true)]
    async fn the_breaker_opens_at_zero_and_probes_after_the_cooldown() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        assert!(gate.try_acquire(T).is_none());

        let start = Instant::now();
        let probe = gate.acquire(T).await;
        assert_eq!(Instant::now() - start, COOLDOWN);
        assert!(matches!(state(&gate).3, Breaker::Probing { .. }));
        assert!(gate.try_acquire(T).is_none(), "exactly one probe");
        probe.finish(Outcome::Success);
        assert_eq!(state(&gate), (1, 0, 0, Breaker::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_probe_answered_with_another_4xx_closes_the_breaker() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        gate.acquire(T).await.finish(Outcome::Refused);
        assert_eq!(state(&gate), (1, 0, 0, Breaker::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_probe_doubles_the_open_period() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        gate.acquire(T).await.finish(Outcome::Overloaded);
        let start = Instant::now();
        let probe = gate.acquire(T).await;
        assert_eq!(Instant::now() - start, COOLDOWN * 2);
        probe.finish(Outcome::Overloaded);
        let start = Instant::now();
        gate.acquire(T).await.finish(Outcome::Success);
        assert_eq!(Instant::now() - start, COOLDOWN * 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_probe_waits_out_its_held_slot() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        gate.acquire(T).await.finish(Outcome::TimedOut);
        let start = Instant::now();
        let probe = gate.acquire(T).await;
        assert_eq!(
            Instant::now() - start,
            TIMEOUT_HOLD,
            "the hold, not the doubled cooldown, sets the recovery time"
        );
        probe.finish(Outcome::Success);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_probe_lets_the_next_waiter_probe_at_once() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        drop(gate.acquire(T).await);
        let start = Instant::now();
        let probe = gate.acquire(T).await;
        assert_eq!(Instant::now(), start);
        assert!(probe.probe);
        probe.finish(Outcome::Success);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_resets_the_doubling() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        gate.acquire(T).await.finish(Outcome::Overloaded);
        gate.acquire(T).await.finish(Outcome::Overloaded);
        gate.acquire(T).await.finish(Outcome::Success);
        assert_eq!(state(&gate).3, Breaker::Closed);

        gate.acquire(T).await.finish(Outcome::Overloaded);
        assert!(is_open(&gate));
        let start = Instant::now();
        gate.acquire(T).await.finish(Outcome::Success);
        assert_eq!(Instant::now() - start, COOLDOWN);
    }

    #[test]
    fn the_cooldown_is_capped() {
        assert_eq!(cooldown(0), COOLDOWN);
        assert_eq!(cooldown(4), COOLDOWN * 16);
        assert_eq!(cooldown(5), COOLDOWN_MAX);
        assert_eq!(cooldown(u32::MAX), COOLDOWN_MAX);
    }

    #[tokio::test(start_paused = true)]
    async fn an_in_flight_request_finishing_while_open_changes_nothing() {
        let gate = HomeserverGate::for_test();
        let a = gate.acquire(T).await;
        let b = gate.acquire(T).await;
        a.finish(Outcome::Overloaded);
        // `b` fills the lowered limit, so open the breaker with a long-poll.
        gate.long_poll().await.finish(Outcome::Overloaded);
        assert!(is_open(&gate));
        b.finish(Outcome::Success);
        assert_eq!(state(&gate).0, 0);
        assert!(is_open(&gate));
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_failures_halve_but_never_open_the_breaker() {
        let gate = HomeserverGate::for_test();
        gate.report_interactive(gate.epoch(), Outcome::TimedOut);
        assert_eq!(
            state(&gate),
            (1, 0, 0, Breaker::Closed),
            "halved, holding no slot"
        );
        gate.report_interactive(gate.epoch(), Outcome::Overloaded);
        assert_eq!(state(&gate), (1, 0, 0, Breaker::Closed), "floor of 1");
        for _ in 0..GROW_AFTER * 2 {
            gate.report_interactive(gate.epoch(), Outcome::Success);
        }
        assert_eq!(state(&gate).0, 1, "interactive successes never grow it");
    }

    #[tokio::test(start_paused = true)]
    async fn interactive_reports_are_ignored_while_open() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        let before = state(&gate);
        gate.report_interactive(gate.epoch(), Outcome::TimedOut);
        assert_eq!(state(&gate), before);
    }

    #[tokio::test(start_paused = true)]
    async fn permits_are_first_in_first_out() {
        let gate = HomeserverGate::for_test();
        let a = gate.acquire(T).await;
        let b = gate.acquire(T).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut waiters = Vec::new();
        for n in 0..3 {
            let gate = Arc::clone(&gate);
            let order = Arc::clone(&order);
            waiters.push(tokio::spawn(async move {
                let permit = gate.acquire(T).await;
                order.lock().unwrap().push(n);
                tokio::time::sleep(Duration::from_millis(1)).await;
                permit.finish(Outcome::Success);
            }));
            // Queue them in a known order.
            tokio::task::yield_now().await;
        }
        assert!(gate.try_acquire(T).is_none(), "waiters are queued");
        a.finish(Outcome::Success);
        b.finish(Outcome::Success);
        for waiter in waiters {
            waiter.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_waiter_leaves_the_queue() {
        let gate = HomeserverGate::for_test();
        let a = gate.acquire(T).await;
        let b = gate.acquire(T).await;
        let first = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(T).await.finish(Outcome::Success) }
        });
        tokio::task::yield_now().await;
        let second = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.acquire(T).await.finish(Outcome::Success) }
        });
        tokio::task::yield_now().await;
        first.abort();
        let _ = first.await;
        a.finish(Outcome::Success);
        second.await.unwrap();
        b.finish(Outcome::Success);
        assert_eq!(gate.snapshot().waiting, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_permit_frees_its_slot_and_teaches_nothing() {
        let gate = HomeserverGate::for_test();
        drop(gate.acquire(T).await);
        gate.acquire(T).await.finish(Outcome::Local);
        assert_eq!(state(&gate), (INITIAL_LIMIT, 0, 0, Breaker::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_poll_holds_no_slot_and_its_latency_is_unclassified() {
        let gate = HomeserverGate::for_test();
        for _ in 0..GROW_AFTER * 2 {
            let poll = gate.long_poll().await;
            assert!(
                gate.try_acquire(T).is_some(),
                "a poll in flight holds no slot"
            );
            poll.finish(Outcome::Success);
        }
        assert_eq!(state(&gate).0, INITIAL_LIMIT, "polls never grow the limit");
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_long_poll_halves_and_holds_a_slot() {
        let gate = HomeserverGate::for_test();
        gate.long_poll().await.finish(Outcome::TimedOut);
        assert_eq!(state(&gate), (1, 0, 1, Breaker::Closed));
        assert!(
            gate.try_acquire(T).is_none(),
            "its held slot fills the limit"
        );
        gate.long_poll().await.finish(Outcome::Overloaded);
        assert!(is_open(&gate), "a long-poll can open the breaker");
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_poll_waits_while_the_breaker_is_open_and_probes_when_nobody_else_does() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        let start = Instant::now();
        let poll = gate.long_poll().await;
        assert_eq!(Instant::now() - start, COOLDOWN);
        assert!(poll.probe);
        poll.finish(Outcome::Success);
        assert_eq!(state(&gate), (1, 0, 0, Breaker::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_background_request_probes_before_a_long_poll() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        let background = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                let permit = gate.acquire(T).await;
                let probe = permit.probe;
                tokio::time::sleep(Duration::from_secs(1)).await;
                permit.finish(Outcome::Success);
                probe
            }
        });
        tokio::task::yield_now().await;
        let poll = gate.long_poll().await;
        assert!(background.await.unwrap(), "the queued request probed");
        assert!(
            !poll.probe,
            "the poll waited for the probe to close the breaker"
        );
        poll.finish(Outcome::Success);
    }

    #[tokio::test(start_paused = true)]
    async fn the_snapshot_reports_the_open_breaker_and_its_next_probe() {
        let gate = HomeserverGate::for_test();
        open_on_overload(&gate).await;
        let snapshot = gate.snapshot();
        assert_eq!((snapshot.limit, snapshot.active, snapshot.held), (0, 0, 0));
        let BreakerSnapshot::Open {
            opened_at,
            next_probe_at,
        } = snapshot.breaker
        else {
            panic!("breaker should be open: {snapshot:?}");
        };
        assert_eq!(next_probe_at.duration_since(opened_at).unwrap(), COOLDOWN);
    }

    #[test]
    fn the_key_is_the_origin_with_an_explicit_port() {
        let gates = HomeserverGates::default();
        let a = gates.for_url(&Url::parse("https://Matrix.Example.org/").unwrap());
        let b = gates.for_url(&Url::parse("https://matrix.example.org:443/_matrix").unwrap());
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.key, "https://matrix.example.org:443");
        let c = gates.for_url(&Url::parse("http://matrix.example.org:8008").unwrap());
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(gates.snapshot().len(), 2);
    }
}
