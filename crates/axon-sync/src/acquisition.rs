//! Paced acquisition of room details from an account's homeserver (ADR 0126
//! decisions 2-4 and 6).
//!
//! Reads never wait for the homeserver: they ask the account's worker for a
//! job through [`RoomDetailAcquisitions`] and return what is cached. Each
//! account run owns one worker, which takes one job at a time from a bounded
//! queue keyed by (room, job), sends at most one request every
//! [`JOB_INTERVAL`], and takes a [`HomeserverGate`] permit for each. Results
//! commit only while the account is still joined in the membership epoch the
//! job started in, and announce a `room_details.changed` frame when they
//! changed something a client reads.
//!
//! Requests go out through reqwest with the account's current access token
//! rather than the SDK's `send`, which buffers and parses the whole body; the
//! body is read through the shared bounded reader instead. Token refresh stays
//! with the account's sync, so a 401 is a failure to back off from.
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use async_trait::async_trait;
use axon_core::{room_details::ABSENCE_ORDER, JobActivity, RoomDetailJob, RoomDetailSource};
use axon_store::{DetailKind, DetailOutcome, DetailWrite, Store};
use matrix_sdk::{
    ruma::{OwnedRoomAliasId, OwnedRoomId, RoomId},
    Client,
};
use serde_json::Value;
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::bounded_body::{read_bounded_body, BoundedBodyError};
use crate::homeserver_gate::{HomeserverGate, Outcome};
use crate::room_details::RoomDetailsChanges;

/// Jobs one account may have waiting. Matches the member-count and redaction
/// hint queues; about ten rooms' panels.
pub(crate) const QUEUE_CAPACITY: usize = 32;
/// The least time between the starts of two requests from one account.
pub(crate) const JOB_INTERVAL: Duration = Duration::from_millis(250);
/// One request's deadline, from when it has a gate permit. These responses are
/// tiny; a slower answer is load.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest successful response body read; larger ones are refused before
/// parsing.
pub(crate) const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// The largest error body read for its `errcode`.
const MAX_ERROR_BYTES: usize = 4 * 1024;
/// A local alias list longer than this is withheld as `too_large`, not cut.
pub(crate) const MAX_LOCAL_ALIASES: usize = 256;
/// How long an `available`, `partial`, or `too_large` answer stays fresh.
pub(crate) const AVAILABLE_TTL: Duration = Duration::from_secs(15 * 60);
/// How long `forbidden` and `not_found` stay fresh. Also how long a `200`
/// from the state endpoint (a sync coverage gap) waits before asking again.
pub(crate) const CONCLUSIVE_DENIAL_TTL: Duration = Duration::from_secs(60 * 60);
/// How long an absence confirmation stays valid without re-confirming.
pub(crate) const ABSENCE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long `unsupported` stays fresh: a homeserver gains an endpoint only on
/// upgrade.
pub(crate) const UNSUPPORTED_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// The first failure's backoff; it doubles per consecutive failure.
pub(crate) const FAILURE_BACKOFF_MIN: Duration = Duration::from_secs(60);
/// The longest failure backoff, matching ADR 0122's cooldown cap.
pub(crate) const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);
/// The longest `Retry-After` honoured; a larger one is clamped.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// Queue and registry
// ---------------------------------------------------------------------------

struct Entry {
    room_id: OwnedRoomId,
    job: RoomDetailJob,
    /// A read reported this job as queued or running, so a client is showing
    /// that and needs a frame when it ends even if nothing changed.
    observed: bool,
}

impl Entry {
    fn is(&self, room_id: &str, job: RoomDetailJob) -> bool {
        self.job == job && self.room_id.as_str() == room_id
    }
}

#[derive(Default)]
struct QueueState {
    queued: VecDeque<Entry>,
    /// The job the worker has taken. It reads as `queued` while it waits for
    /// its pacing interval or a gate permit, and as `running` only while a
    /// request is in flight (`sending`).
    running: Option<Entry>,
    sending: bool,
    /// Jobs whose last run could not read or record the cache, with when they
    /// may queue again. A read cannot turn such a job straight into another
    /// homeserver request; bounded by [`COOLDOWN_CAPACITY`].
    cooldown: VecDeque<(OwnedRoomId, RoomDetailJob, Instant)>,
}

/// The most jobs held back after a local failure; the oldest is forgotten
/// first.
const COOLDOWN_CAPACITY: usize = 2 * QUEUE_CAPACITY;

/// One account's job queue. Queue membership and activity live in memory
/// only, so a restart cannot leave a job permanently pending.
#[derive(Default)]
pub(crate) struct Queue {
    state: Mutex<QueueState>,
    wake: Notify,
    /// Where a dropped job that a read saw is announced.
    changes: Option<RoomDetailsChanges>,
}

/// The frame source a client re-reads for `job`.
fn frame_source(job: RoomDetailJob) -> RoomDetailSource {
    match job {
        RoomDetailJob::Absence => RoomDetailSource::Metadata,
        RoomDetailJob::LocalAliases | RoomDetailJob::DirectoryVisibility => {
            RoomDetailSource::HomeserverDetails
        }
    }
}

impl Queue {
    pub(crate) fn new(changes: RoomDetailsChanges) -> Self {
        Self {
            changes: Some(changes),
            ..Self::default()
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        // Plain data, never held across an await or a panic-prone call.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Queue `job` unless it is already queued or running. When the queue is
    /// full the oldest job is dropped: the newest request is the panel a user
    /// is looking at now, and a dropped job is requested again by the next
    /// read of its room. A dropped job that a read saw is announced, so its
    /// panel re-reads instead of showing `queued` until it is reopened.
    /// Returns the dropped job, if any.
    fn push(
        &self,
        room_id: OwnedRoomId,
        job: RoomDetailJob,
    ) -> Option<(OwnedRoomId, RoomDetailJob)> {
        let mut state = self.lock();
        let room = room_id.as_str();
        let now = Instant::now();
        state.cooldown.retain(|(_, _, until)| *until > now);
        if state.running.as_ref().is_some_and(|e| e.is(room, job))
            || state.queued.iter().any(|e| e.is(room, job))
            || state
                .cooldown
                .iter()
                .any(|(id, kind, _)| *kind == job && id.as_str() == room)
        {
            return None;
        }
        let dropped = (state.queued.len() >= QUEUE_CAPACITY)
            .then(|| state.queued.pop_front())
            .flatten();
        state.queued.push_back(Entry {
            room_id,
            job,
            observed: false,
        });
        drop(state);
        self.wake.notify_one();
        let dropped = dropped?;
        if let Some(changes) = self.changes.as_ref().filter(|_| dropped.observed) {
            changes.mark(dropped.room_id.as_str(), frame_source(dropped.job));
        }
        Some((dropped.room_id, dropped.job))
    }

    /// Where `job` stands, recording that a read saw it.
    fn observe(&self, room_id: &str, job: RoomDetailJob) -> JobActivity {
        let mut state = self.lock();
        let sending = state.sending;
        if let Some(entry) = state.running.as_mut().filter(|e| e.is(room_id, job)) {
            entry.observed = true;
            // Waiting for the pacing interval or a gate permit is queueing,
            // never running (ADR 0126 decision 3).
            return if sending {
                JobActivity::Running
            } else {
                JobActivity::Queued
            };
        }
        if let Some(entry) = state.queued.iter_mut().find(|e| e.is(room_id, job)) {
            entry.observed = true;
            return JobActivity::Queued;
        }
        JobActivity::Idle
    }

    /// Wait for the next job and take it. It reads as queued until
    /// [`sending`](Self::sending) marks a request in flight.
    async fn next(&self) -> (OwnedRoomId, RoomDetailJob) {
        loop {
            {
                let mut state = self.lock();
                if let Some(entry) = state.queued.pop_front() {
                    let next = (entry.room_id.clone(), entry.job);
                    state.running = Some(entry);
                    return next;
                }
            }
            // `notify_one` stores a permit when nobody waits, so a push
            // between the look and this wait still wakes it.
            self.wake.notified().await;
        }
    }

    /// Whether the taken job has a request in flight: set once it holds a
    /// gate permit, cleared when the request ends.
    fn sending(&self, sending: bool) {
        self.lock().sending = sending;
    }

    /// End the taken job; whether a read saw it. A job that could not read
    /// or record the cache is held back for [`FAILURE_BACKOFF_MIN`], so the
    /// frame that answers its observer cannot requeue it at once.
    fn finish(&self, local_failure: bool) -> bool {
        let mut state = self.lock();
        state.sending = false;
        let Some(entry) = state.running.take() else {
            return false;
        };
        if local_failure {
            if state.cooldown.len() >= COOLDOWN_CAPACITY {
                state.cooldown.pop_front();
            }
            state.cooldown.push_back((
                entry.room_id,
                entry.job,
                Instant::now() + FAILURE_BACKOFF_MIN,
            ));
        }
        entry.observed
    }

    fn depth(&self) -> usize {
        self.lock().queued.len()
    }
}

/// Every running account's acquisition queue. Cheap to clone; clones share
/// it. Requests for an account whose run is not active are dropped, and its
/// jobs report idle.
#[derive(Clone, Default)]
pub struct RoomDetailAcquisitions(Arc<Mutex<HashMap<Uuid, Arc<Queue>>>>);

impl std::fmt::Debug for RoomDetailAcquisitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomDetailAcquisitions")
            .finish_non_exhaustive()
    }
}

impl RoomDetailAcquisitions {
    fn queue(&self, account_id: Uuid) -> Option<Arc<Queue>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&account_id)
            .cloned()
    }

    /// Ask `account_id`'s worker for `job` on `room_id`. Never waits. The
    /// caller has already checked that the job is due and the room eligible;
    /// the worker checks both again before any request.
    pub fn request(&self, account_id: Uuid, room_id: &str, job: RoomDetailJob) {
        let Ok(room_id) = OwnedRoomId::try_from(room_id) else {
            return;
        };
        let Some(queue) = self.queue(account_id) else {
            return;
        };
        if let Some((dropped_room, dropped_job)) = queue.push(room_id, job) {
            tracing::debug!(target: "axon_sync::acquisition", %account_id, room_id = %dropped_room, kind = ?dropped_job, "acquisition queue full; dropped its oldest job");
        }
    }

    /// Where `job` on `room_id` stands in `account_id`'s worker.
    pub fn activity(&self, account_id: Uuid, room_id: &str, job: RoomDetailJob) -> JobActivity {
        self.queue(account_id)
            .map_or(JobActivity::Idle, |queue| queue.observe(room_id, job))
    }

    /// Give `account_id` a fresh queue for one account run. Dropping the
    /// registration removes it, unless a later run has replaced it.
    pub(crate) fn register(&self, account_id: Uuid, changes: RoomDetailsChanges) -> Registration {
        let queue = Arc::new(Queue::new(changes));
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(account_id, Arc::clone(&queue));
        Registration {
            registry: self.clone(),
            account_id,
            queue,
        }
    }
}

/// One account run's place in [`RoomDetailAcquisitions`].
pub(crate) struct Registration {
    registry: RoomDetailAcquisitions,
    account_id: Uuid,
    queue: Arc<Queue>,
}

impl Registration {
    pub(crate) fn queue(&self) -> Arc<Queue> {
        Arc::clone(&self.queue)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut accounts = self
            .registry
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if accounts
            .get(&self.account_id)
            .is_some_and(|queue| Arc::ptr_eq(queue, &self.queue))
        {
            accounts.remove(&self.account_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Why a request got no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoAnswer {
    Timeout,
    Transport,
    /// No access token to send.
    Unauthorized,
}

/// What the homeserver said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reply {
    /// `200`, with the body when it was asked for.
    Ok(Vec<u8>),
    /// `200` whose body exceeded [`MAX_RESPONSE_BYTES`].
    TooLarge,
    /// Any other status, with the error body's `errcode` and the longer of
    /// `Retry-After` and `retry_after_ms`.
    Status {
        status: u16,
        errcode: Option<String>,
        retry_after_ms: Option<i64>,
    },
    Failed(NoAnswer),
}

/// Where requests go. The production one is [`Homeserver`]; tests script it.
#[async_trait]
pub(crate) trait Upstream: Send + Sync {
    /// `GET` the client-server API path made of `segments`, reading a `200`
    /// body only when `read_body` is set. Calls `admitted` once the request
    /// may go out, after any wait for a gate permit.
    async fn get(&self, segments: &[&str], read_body: bool, admitted: &(dyn Fn() + Sync)) -> Reply;
}

/// The account's homeserver, behind its load gate.
pub(crate) struct Homeserver {
    client: Client,
    http: reqwest::Client,
    gate: Arc<HomeserverGate>,
}

impl Homeserver {
    /// Fails only if the HTTP client cannot initialize (TLS, say); the caller
    /// then runs the account without acquisition.
    pub(crate) fn new(client: Client, gate: Arc<HomeserverGate>) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client,
            http: http_client()?,
            gate,
        })
    }
}

/// Match the SDK's production HTTP defaults, without redirects, so the body
/// limit and the access token stay with the homeserver.
fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(crate::client::user_agent())
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .build()
}

fn endpoint(homeserver: &Url, segments: &[&str]) -> Option<Url> {
    let mut url = homeserver.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["_matrix", "client", "v3"])
        .extend(segments);
    Some(url)
}

#[async_trait]
impl Upstream for Homeserver {
    async fn get(&self, segments: &[&str], read_body: bool, admitted: &(dyn Fn() + Sync)) -> Reply {
        // Waiting for a permit is not part of the request's timeout, and a
        // closed gate is never a failure: the job still reads as queued.
        let permit = self.gate.acquire(REQUEST_TIMEOUT).await;
        admitted();
        let Some(token) = self.client.access_token() else {
            permit.finish(Outcome::Local);
            return Reply::Failed(NoAnswer::Unauthorized);
        };
        let Some(url) = endpoint(&self.client.homeserver(), segments) else {
            permit.finish(Outcome::Local);
            return Reply::Failed(NoAnswer::Transport);
        };
        let (reply, outcome) =
            match tokio::time::timeout(REQUEST_TIMEOUT, send(&self.http, url, &token, read_body))
                .await
            {
                Ok(reply) => {
                    let outcome = gate_outcome(&reply);
                    (reply, outcome)
                }
                Err(_) => (Reply::Failed(NoAnswer::Timeout), Outcome::TimedOut),
            };
        permit.finish(outcome);
        reply
    }
}

fn gate_outcome(reply: &Reply) -> Outcome {
    match reply {
        Reply::Ok(_) => Outcome::Success,
        Reply::TooLarge => Outcome::Refused,
        Reply::Status { status, .. } if *status == 429 || *status >= 500 => Outcome::Overloaded,
        Reply::Status { .. } => Outcome::Refused,
        Reply::Failed(NoAnswer::Timeout) => Outcome::TimedOut,
        Reply::Failed(NoAnswer::Transport) => Outcome::Overloaded,
        Reply::Failed(NoAnswer::Unauthorized) => Outcome::Local,
    }
}

async fn send(http: &reqwest::Client, url: Url, token: &str, read_body: bool) -> Reply {
    let response = match http.get(url).bearer_auth(token).send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return Reply::Failed(NoAnswer::Timeout),
        Err(_) => return Reply::Failed(NoAnswer::Transport),
    };
    let status = response.status().as_u16();
    if status == 200 {
        if !read_body {
            return Reply::Ok(Vec::new());
        }
        return match read_bounded_body(response, MAX_RESPONSE_BYTES).await {
            Ok(body) => Reply::Ok(body),
            Err(BoundedBodyError::TooLarge) => Reply::TooLarge,
            Err(BoundedBodyError::Transport(error)) if error.is_timeout() => {
                Reply::Failed(NoAnswer::Timeout)
            }
            Err(BoundedBodyError::Transport(_)) => Reply::Failed(NoAnswer::Transport),
        };
    }
    let header_retry = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| retry_after(value, chrono::Utc::now()));
    let body: Option<Value> = read_bounded_body(response, MAX_ERROR_BYTES)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice(&body).ok());
    let errcode = body
        .as_ref()
        .and_then(|body| body.get("errcode"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let body_retry = body
        .as_ref()
        .and_then(|body| body.get("retry_after_ms"))
        .and_then(Value::as_i64);
    let retry_after_ms = header_retry
        .max(body_retry)
        .map(|retry| retry.clamp(0, ms(MAX_RETRY_AFTER)));
    Reply::Status {
        status,
        errcode,
        retry_after_ms,
    }
}

/// A `Retry-After` value as milliseconds from `now`: delta-seconds, or an
/// HTTP-date (IMF-fixdate). A date in the past is no delay.
fn retry_after(value: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(ms(Duration::from_secs(seconds)));
    }
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some((at.timestamp_millis() - now.timestamp_millis()).max(0))
}

// ---------------------------------------------------------------------------
// Interpretation
// ---------------------------------------------------------------------------

fn answer(status: &'static str, value: Option<Value>, ttl: Duration) -> DetailOutcome {
    DetailOutcome::Answer {
        status,
        value,
        ttl_ms: ms(ttl),
    }
}

fn failure(category: &'static str, retry_after_ms: Option<i64>) -> DetailOutcome {
    DetailOutcome::Failure {
        category,
        retry_after_ms,
        min_backoff_ms: ms(FAILURE_BACKOFF_MIN),
        max_backoff_ms: ms(FAILURE_BACKOFF_MAX),
    }
}

/// What the cache records for `reply` to a `kind` request.
pub(crate) fn interpret(kind: DetailKind, reply: Reply) -> DetailOutcome {
    let invalid = || failure("invalid_response", None);
    match reply {
        Reply::Failed(NoAnswer::Timeout) => failure("timeout", None),
        Reply::Failed(NoAnswer::Transport) => failure("transport", None),
        Reply::Failed(NoAnswer::Unauthorized) => failure("unauthorized", None),
        Reply::TooLarge if kind == DetailKind::Absence => invalid(),
        Reply::TooLarge => answer("too_large", None, AVAILABLE_TTL),
        Reply::Ok(_) if kind == DetailKind::Absence => DetailOutcome::Present {
            retry_in_ms: ms(CONCLUSIVE_DENIAL_TTL),
        },
        Reply::Ok(body) => serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|body| answer_body(kind, &body))
            .unwrap_or_else(invalid),
        Reply::Status {
            status,
            errcode,
            retry_after_ms,
        } => match (status, errcode.as_deref()) {
            (401, _) => failure("unauthorized", retry_after_ms),
            (403, _) => answer("forbidden", None, CONCLUSIVE_DENIAL_TTL),
            (404, Some("M_NOT_FOUND")) if kind == DetailKind::Absence => {
                answer("absent", None, ABSENCE_TTL)
            }
            (404, Some("M_NOT_FOUND")) => answer("not_found", None, CONCLUSIVE_DENIAL_TTL),
            // The endpoint is missing: the server, or a proxy in front of it,
            // does not serve it. That settles a details read; it says nothing
            // about whether a state tuple exists.
            (404, None | Some("M_UNRECOGNIZED")) | (405, _) => {
                if kind == DetailKind::Absence {
                    invalid()
                } else {
                    answer("unsupported", None, UNSUPPORTED_TTL)
                }
            }
            (429, _) => failure("rate_limited", retry_after_ms),
            (500..=599, _) => failure("server_error", retry_after_ms),
            _ => invalid(),
        },
    }
}

/// A `200` body's answer, or `None` when it has the wrong shape.
fn answer_body(kind: DetailKind, body: &Value) -> Option<DetailOutcome> {
    match kind {
        DetailKind::LocalAliases => {
            let aliases = body.get("aliases")?.as_array()?;
            if aliases.len() > MAX_LOCAL_ALIASES {
                return Some(answer("too_large", None, AVAILABLE_TTL));
            }
            let valid: Vec<Value> = aliases
                .iter()
                .filter_map(Value::as_str)
                .filter(|alias| OwnedRoomAliasId::try_from(*alias).is_ok())
                .map(|alias| Value::String(alias.to_owned()))
                .collect();
            let status = if valid.len() == aliases.len() {
                "available"
            } else {
                "partial"
            };
            Some(answer(status, Some(Value::Array(valid)), AVAILABLE_TTL))
        }
        DetailKind::DirectoryVisibility => {
            let visibility = body.get("visibility")?.as_str()?;
            matches!(visibility, "public" | "private").then(|| {
                answer(
                    "available",
                    Some(serde_json::json!({ "visibility": visibility })),
                    AVAILABLE_TTL,
                )
            })
        }
        DetailKind::Absence => None,
    }
}

/// A short, allowlisted name for an outcome, for logs.
fn outcome_label(outcome: &DetailOutcome) -> &'static str {
    match outcome {
        DetailOutcome::Answer { status, .. } => status,
        DetailOutcome::Present { .. } => "present",
        DetailOutcome::Failure { category, .. } => category,
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// One account's acquisition worker.
pub(crate) struct Worker<U> {
    upstream: U,
    store: Store,
    account_id: Uuid,
    room_details: RoomDetailsChanges,
    queue: Arc<Queue>,
    last_request: Option<Instant>,
}

impl<U: Upstream> Worker<U> {
    pub(crate) fn new(
        upstream: U,
        store: Store,
        account_id: Uuid,
        room_details: RoomDetailsChanges,
        queue: Arc<Queue>,
    ) -> Self {
        Self {
            upstream,
            store,
            account_id,
            room_details,
            queue,
            last_request: None,
        }
    }

    /// Take jobs until canceled.
    pub(crate) async fn run(mut self, cancel: CancellationToken) {
        loop {
            let (room_id, job) = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                next = self.queue.next() => next,
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                () = self.step(&room_id, job) => {}
            }
        }
    }

    /// Run the job [`Queue::next`] just took, end it, and announce it when it
    /// changed something or a read saw it queued or running.
    async fn step(&mut self, room_id: &RoomId, job: RoomDetailJob) {
        let ran = self.run_job(room_id, job).await;
        let observed = self.queue.finish(ran.local_failure);
        // An observer hears the job ended even after a local failure: the
        // job is held back, so its re-read sees `idle` rather than queueing
        // another request.
        if ran.changed || observed {
            self.room_details.mark(room_id.as_str(), frame_source(job));
        }
    }

    /// Run one job.
    async fn run_job(&mut self, room_id: &RoomId, job: RoomDetailJob) -> Ran {
        let account_id = self.account_id;
        let mut ran = Ran::default();
        let epoch = match self.store.joined_epoch(account_id, room_id.as_str()).await {
            Ok(Some(epoch)) => epoch,
            Ok(None) => {
                tracing::debug!(target: "axon_sync::acquisition", %account_id, %room_id, kind = ?job, outcome = "not_joined", "skipped room detail acquisition");
                return ran;
            }
            Err(error) => {
                tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = ?job, reason = error.diagnostic_reason(), "room detail acquisition could not read membership");
                ran.local_failure = true;
                return ran;
            }
        };
        match job {
            RoomDetailJob::LocalAliases | RoomDetailJob::DirectoryVisibility => {
                let (kind, segments) = match job {
                    RoomDetailJob::LocalAliases => (
                        DetailKind::LocalAliases,
                        vec!["rooms", room_id.as_str(), "aliases"],
                    ),
                    _ => (
                        DetailKind::DirectoryVisibility,
                        vec!["directory", "list", "room", room_id.as_str()],
                    ),
                };
                // A duplicate request can queue while an earlier job commits;
                // ask only while the detail is still due.
                match self
                    .store
                    .room_detail_snapshot(account_id, room_id.as_str(), &[kind])
                    .await
                {
                    Ok(snapshot) if snapshot.rows.first().is_some_and(|row| !row.due) => {
                        return ran
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = kind.as_str(), reason = error.diagnostic_reason(), "room detail acquisition could not read its cache");
                        ran.local_failure = true;
                        return ran;
                    }
                }
                ran.record(
                    self.request(room_id, epoch, kind, "", &segments)
                        .await
                        .write,
                );
                ran
            }
            RoomDetailJob::Absence => {
                let types = match self
                    .store
                    .due_absence_types(account_id, room_id.as_str(), &ABSENCE_ORDER)
                    .await
                {
                    Ok(types) => types,
                    Err(error) => {
                        tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = "absence", reason = error.diagnostic_reason(), "room detail acquisition could not read its cache");
                        ran.local_failure = true;
                        return ran;
                    }
                };
                for state_type in &types {
                    let segments = ["rooms", room_id.as_str(), "state", state_type.as_str(), ""];
                    let Requested { write, failed } = self
                        .request(room_id, epoch, DetailKind::Absence, state_type, &segments)
                        .await;
                    ran.record(write);
                    // A failed request ends the job: the failed type backs off,
                    // and the rest stay due for the next read. Asking on would
                    // send up to seven more requests to a homeserver that is
                    // rate limiting, failing, or timing out, and hold this
                    // account's worker for each.
                    if failed || write.is_none() {
                        break;
                    }
                    // State that arrived during the request fences only its
                    // own type; a membership change fences the rest. A failed
                    // membership read is not a membership change.
                    if write == Some(DetailWrite::Fenced) {
                        match self.store.joined_epoch(account_id, room_id.as_str()).await {
                            Ok(current) if current != Some(epoch) => break,
                            Ok(_) => {}
                            Err(error) => {
                                tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = "absence", reason = error.diagnostic_reason(), "room detail acquisition could not re-read membership; continuing");
                            }
                        }
                    }
                }
                ran
            }
        }
    }

    /// Send one paced request and record its outcome.
    async fn request(
        &mut self,
        room_id: &RoomId,
        epoch: i64,
        kind: DetailKind,
        state_type: &str,
        segments: &[&str],
    ) -> Requested {
        if let Some(last) = self.last_request {
            tokio::time::sleep_until(last + JOB_INTERVAL).await;
        }
        // The job reads as running only from admission, the request's
        // elapsed time excludes the wait for a gate permit, and the next
        // request's interval counts from when this one was sent.
        let admitted_at = Mutex::new(None);
        let queue = Arc::clone(&self.queue);
        let reply = self
            .upstream
            .get(segments, kind != DetailKind::Absence, &|| {
                *admitted_at
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
                queue.sending(true);
            })
            .await;
        self.queue.sending(false);
        let admitted_at = admitted_at
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A request the gate never admitted was not sent; pace from now.
        self.last_request = Some(admitted_at.unwrap_or_else(Instant::now));
        let elapsed_ms = admitted_at.map_or(0, |at| at.elapsed().as_millis() as u64);
        let outcome = interpret(kind, reply);
        let failed = matches!(outcome, DetailOutcome::Failure { .. });
        let account_id = self.account_id;
        let label = outcome_label(&outcome);
        let queue_depth = self.queue.depth();
        let kind_name = kind.as_str();
        if matches!(outcome, DetailOutcome::Present { .. }) {
            tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = kind_name, state_type, "the homeserver has state that sync never delivered (sync coverage gap)");
        }
        let write = match self
            .store
            .record_room_detail(
                account_id,
                room_id.as_str(),
                epoch,
                kind,
                state_type,
                &outcome,
            )
            .await
        {
            Ok(write) => write,
            Err(error) => {
                tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = kind_name, state_type, outcome = label, reason = error.diagnostic_reason(), "room detail acquisition could not record its result");
                return Requested {
                    write: None,
                    failed,
                };
            }
        };
        tracing::debug!(target: "axon_sync::acquisition", %account_id, %room_id, kind = kind_name, state_type, outcome = label, elapsed_ms, queue_depth, write = ?write, "room detail acquisition finished");
        // A failure that leaves the row unchanged repeats the last one.
        if failed && write == DetailWrite::Unchanged {
            tracing::warn!(target: "axon_sync::acquisition", %account_id, %room_id, kind = kind_name, state_type, outcome = label, elapsed_ms, "room detail acquisition keeps failing; backing off");
        }
        Requested {
            write: Some(write),
            failed,
        }
    }
}

/// What one request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Requested {
    /// The cache write; `None` when the outcome could not be recorded.
    write: Option<DetailWrite>,
    /// The homeserver gave no conclusive answer.
    failed: bool,
}

/// What one job did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Ran {
    /// Something a client reads changed.
    changed: bool,
    /// The cache could not be read or a result could not be recorded, so no
    /// backoff was stored; the queue holds the job back instead.
    local_failure: bool,
}

impl Ran {
    fn record(&mut self, write: Option<DetailWrite>) {
        match write {
            Some(DetailWrite::Changed) => self.changed = true,
            Some(_) => {}
            None => self.local_failure = true,
        }
    }
}

#[cfg(test)]
mod tests;
