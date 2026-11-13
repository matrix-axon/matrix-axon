//! The re-decryption queue: back-fill UTDs once their megolm keys arrive.
//!
//! A fresh, unverified `axon` device cannot decrypt historical encrypted
//! messages, so the persistence handler stores them as UTDs — `m.room.encrypted`
//! rows with `content = NULL`, the ciphertext (incl. `session_id`) preserved in
//! `raw_event`. When the corresponding megolm room keys later reach the SDK
//! crypto store, this module finds the matching rows, asks the SDK to decrypt
//! the stored ciphertext, and back-fills `content` + the real `event_type`.
//!
//! Two drivers feed it:
//!
//! - [`run`] subscribes to the SDK's `room_keys_received_stream`: every batch of
//!   arriving keys names a `(room_id, session_id)`, and we drain exactly the
//!   pending rows waiting on that session.
//! - [`sweep_pending_utds`] runs at startup and on explicit retries. The default
//!   startup mode attempts each stored UTD once; explicit retries and the legacy
//!   startup mode retry the full pending backlog.
//!
//! Every per-row failure (a row that still won't decrypt, a malformed envelope,
//! a missing room) is logged and skipped — re-decryption is best-effort
//! back-fill and must never take down the sync task. See ADR 0014.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axon_search::IndexHandle;
use axon_store::{PendingUtd, Store};
use futures_util::stream::BoxStream;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::deserialized_responses::{EncryptionInfo, TimelineEvent, TimelineEventKind};
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::AnyTimelineEvent;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{OwnedRoomId, RoomId};
use matrix_sdk::{Client, Room};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::homeserver_gate::{Caller, GateError, HomeserverGate, Outcome};
use tokio::time::error::Elapsed;

/// Bound on one room's key-backup download, retries and the local key import
/// included: the SDK's own 30 s timeout applies to each HTTP attempt, not the
/// call (ADR 0122 decision 1).
///
/// Longer than one attempt because the import can be what runs long. On
/// 2026-11-08 (#1005) Synapse served a room's keys in 13 ms, and the call still
/// had not returned 30 s later, during the account's first sync. Reaching this
/// bound halves the gate's limit but holds no slot (see [`backup_outcome`]). A
/// sweep's cancellation still ends the download at once ([`unless_cancelled`]),
/// so recover keeps its own shorter deadline.
const BACKUP_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Which pending rows an account-wide sweep should load.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SweepScope {
    /// Default boot behavior: attempt only rows that have not yet had a startup
    /// retry. Fresh key arrivals still use the session stream regardless.
    StartupUnattempted,
    /// Operator/config/recovery behavior: retry every currently pending UTD.
    AllPending,
}

/// Counters from one account-wide re-decryption sweep.
#[derive(Debug, Clone, Default)]
pub struct RedecryptSummary {
    /// Rows selected before the sweep started.
    pub selected: usize,
    /// Rows that reached `decrypt_event` or were identified as malformed.
    pub attempted: usize,
    /// Rows successfully back-filled.
    pub decrypted: usize,
    /// Rows selected but still UTD after the sweep finished or timed out.
    pub still_pending: usize,
    /// Rows marked so the default startup sweep will not select them again.
    pub startup_marked: u64,
    /// Whether the caller stopped waiting before the sweep completed.
    pub timed_out: bool,
    /// Event IDs that actually reached `redecrypt_one` (i.e. excludes rows
    /// skipped for a missing room handle). Only these are safe to mark as
    /// startup-attempted; a skipped row must remain eligible for retry on the
    /// next boot.
    attempted_event_ids: Vec<String>,
}

impl RedecryptSummary {
    /// Preserve counts from an in-flight sweep and mark it timed out (ADR 0098).
    /// A timeout is not `selected=0`.
    pub fn timed_out(mut summary: Self) -> Self {
        summary.timed_out = true;
        summary.still_pending = summary.selected.saturating_sub(summary.decrypted);
        summary
    }
}

/// Pull the megolm `session_id` out of an `m.room.encrypted` envelope's
/// `content`. Pure; used by the persistence handler to populate the
/// `megolm_session_id` hot column. `None` if the field is absent or non-string.
pub(crate) fn megolm_session_id(raw_event: &Value) -> Option<&str> {
    raw_event.get("content")?.get("session_id")?.as_str()
}

/// Extract `(content, type)` from a re-decrypted event, or `None` if the SDK
/// returned anything other than a successful decryption (it hands back a UTD
/// `TimelineEvent` when no key is available yet — not an error). Pure.
fn extract_decrypted(ev: &TimelineEvent) -> Option<(Value, String)> {
    match &ev.kind {
        TimelineEventKind::Decrypted(decrypted) => extract_content_and_type(&decrypted.event),
        _ => None,
    }
}

/// The SDK's [`EncryptionInfo`] for a successfully re-decrypted event, so the
/// re-decryption path can record the same crypto-provenance siblings the live
/// dispatch path does. `None` for a UTD `TimelineEvent`. Borrows from `ev`.
fn decrypted_encryption_info(ev: &TimelineEvent) -> Option<&EncryptionInfo> {
    match &ev.kind {
        TimelineEventKind::Decrypted(decrypted) => Some(decrypted.encryption_info.as_ref()),
        _ => None,
    }
}

/// Read `content` and `type` out of a cleartext timeline event's JSON. Pure;
/// returns `None` if either field is missing or the wrong shape.
fn extract_content_and_type(raw: &Raw<AnyTimelineEvent>) -> Option<(Value, String)> {
    let content: Value = raw.get_field("content").ok()??;
    let event_type: String = raw.get_field("type").ok()??;
    Some((content, event_type))
}

/// Subscribe to arriving megolm room keys and drain the pending UTDs each one
/// unlocks. Runs until `cancel` fires or the stream closes. `None` from the
/// subscription means there is no `OlmMachine` (not logged in / E2EE off) — we
/// log and return rather than spin.
pub(crate) async fn run(
    client: Client,
    store: Store,
    account_id: Uuid,
    cancel: CancellationToken,
    index: Option<IndexHandle>,
) {
    let stream = match client.encryption().room_keys_received_stream().await {
        Some(stream) => stream,
        None => {
            tracing::warn!(
                %account_id,
                "no olm machine; re-decryption queue idle"
            );
            return;
        }
    };
    tokio::pin!(stream);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            next = stream.next() => match next {
                Some(Ok(infos)) => {
                    for info in infos {
                        redecrypt_session(
                            &client,
                            &store,
                            account_id,
                            info.room_id.as_str(),
                            &info.session_id,
                            index.as_ref(),
                        )
                        .await;
                    }
                }
                // The broadcast channel lagged: we missed some key arrivals. The
                // startup sweep already retried the backlog, and future arrivals
                // still come through, so warn and keep listening.
                Some(Err(err)) => {
                    tracing::warn!(%account_id, error = %err, "room-key stream lagged; some arrivals skipped");
                }
                None => return,
            },
        }
    }
}

/// One startup pass over every pending UTD for the account. Keys already in the
/// crypto store (from a prior run or a just-finished `recover()`) won't fire the
/// arrival stream, so we retry the full backlog once at boot.
///
/// `cancel` is checked between rooms and between rows, and raced against each
/// room's key-backup download while it is in flight ([`unless_cancelled`]), so
/// a recover/manual deadline stops the sweep at once and still returns the
/// partial summary (ADR 0098). The boot path races this against the supervised
/// task's cancel token.
///
/// Each room's key-backup download goes through `backup` (ADR 0122): the boot
/// sweep waits for a permit, while recover and manual re-decryption, which a
/// user is waiting on, never wait and only report their outcome.
pub(crate) async fn sweep_pending_utds(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    scope: SweepScope,
    index: Option<&IndexHandle>,
    cancel: &CancellationToken,
    backup: BackupSource<'_>,
) -> RedecryptSummary {
    let rows = match scope {
        SweepScope::StartupUnattempted => store.pending_utds_for_startup_attempt(account_id).await,
        SweepScope::AllPending => store.pending_utds_for_account(account_id).await,
    };
    let rows = match rows {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(%account_id, error = %err, "failed to load pending UTDs for startup sweep");
            return RedecryptSummary::default();
        }
    };
    if rows.is_empty() {
        return RedecryptSummary::default();
    }
    let selected = rows.len();
    tracing::info!(%account_id, count = selected, ?scope, "sweeping pending UTDs");
    // `from_backup`: recover() imported the backup *decryption key* but not the
    // room keys themselves, so the sweep must pull them from the server backup
    // before decrypting (see redecrypt_rows).
    let mut summary = redecrypt_rows(
        client,
        store,
        account_id,
        rows,
        Some(backup),
        index,
        Some(cancel),
    )
    .await;
    summary.selected = selected;
    summary.still_pending = summary.selected.saturating_sub(summary.decrypted);
    if matches!(scope, SweepScope::StartupUnattempted) {
        // Only mark rows that actually reached `redecrypt_one`: a row skipped
        // because its room hasn't hydrated yet must stay eligible for the next
        // boot's sweep, or it would be stuck as a permanent UTD (see PR #244
        // review).
        match store
            .mark_utd_startup_redecrypt_attempted(account_id, &summary.attempted_event_ids)
            .await
        {
            Ok(marked) => summary.startup_marked = marked,
            Err(err) => {
                tracing::warn!(
                    %account_id,
                    error = %err,
                    "failed to mark UTD startup re-decryption attempts"
                );
            }
        }
    }
    summary
}

/// Drain the pending UTDs in `room_id` waiting on `session_id`.
async fn redecrypt_session(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    session_id: &str,
    index: Option<&IndexHandle>,
) {
    let rows = match store
        .pending_utds_for_session(account_id, room_id, session_id)
        .await
    {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(%account_id, room_id, session_id, error = %err, "failed to load pending UTDs for session");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    // The arrival stream's keys are already in the crypto store — no backup fetch.
    redecrypt_rows(client, store, account_id, rows, None, index, None).await;
}

/// How a sweep's key-backup downloads reach the homeserver: through its gate,
/// as a background or an interactive caller (ADR 0122).
#[derive(Clone, Copy)]
pub(crate) struct BackupSource<'a> {
    pub(crate) gate: &'a Arc<HomeserverGate>,
    pub(crate) caller: Caller,
}

/// Download `room_id`'s megolm keys from the server-side key backup through the
/// homeserver gate (ADR 0122), bounded by [`BACKUP_DOWNLOAD_TIMEOUT`].
///
/// `download_room_keys_for_room` returns `Ok` without sending anything when the
/// crypto store has no backup decryption key or version, and the SDK does not
/// say which happened. So a success teaches the gate nothing: a no-op must not
/// grow its limit, or close its breaker as the probe. While backups are not
/// enabled, the usual reason for the no-op, the download does not wait for a
/// permit either; whatever it sends is reported like an interactive request.
///
/// `are_enabled` is a proxy: the SDK enables backups whenever it stores the key
/// and version together (`recover()`, a received recovery key) and when it
/// resumes them from the store at client start. The one gap is that resume
/// window at boot: a download sent before it finishes goes out without a
/// permit, one per room, reported like an interactive request. The crypto store
/// that holds the exact precondition is not public API.
///
/// Dropping the download at the timeout is safe: matrix-sdk writes the imported
/// keys in one transaction, so at worst this room's rows stay UTD until the
/// next sweep, as on any failed download.
async fn download_backup_keys(
    client: &Client,
    backup: BackupSource<'_>,
    room_id: &RoomId,
) -> Result<Result<(), matrix_sdk::Error>, Elapsed> {
    let backups = client.encryption().backups();
    let caller = if backups.are_enabled().await {
        backup.caller
    } else {
        Caller::Interactive
    };
    gated_backup_download(
        backup.gate,
        caller,
        BACKUP_DOWNLOAD_TIMEOUT,
        backups.download_room_keys_for_room(room_id),
    )
    .await
}

/// Run one key-backup download through `gate` as `caller`, bounded by `bound`
/// and read by [`backup_outcome`].
async fn gated_backup_download(
    gate: &Arc<HomeserverGate>,
    caller: Caller,
    bound: Duration,
    download: impl Future<Output = Result<(), matrix_sdk::Error>>,
) -> Result<Result<(), matrix_sdk::Error>, Elapsed> {
    gate.call_classified(caller, bound, download, backup_outcome)
        .await
}

/// How a key-backup download reads to the gate (ADR 0122, amendment of
/// 2026-11-10).
///
/// - A success is [`Outcome::Local`]: it may have been a no-op that sent
///   nothing (see [`download_backup_keys`]).
/// - Reaching the bound is [`Outcome::Overloaded`]: it halves the limit, and
///   fails the breaker's probe, but **holds no slot**. Which of two causes it
///   was cannot be told apart. One is the client's default request config
///   retrying a 5xx or 429 without limit, which hides the server's errors
///   inside the timeout, so it must still count as load. The other is the
///   local import of the keys into the crypto store, inside the same SDK call
///   (`OlmMachine` is crate-private, so the two can't be timed apart). On
///   2026-11-08 the import was what ran long (#1005). Read as a timeout, it
///   held a slot for `TIMEOUT_HOLD`, which at a limit of 1 stopped all of the
///   account's background work for 15 minutes on a healthy homeserver.
/// - Any other error is read by the gate's own rule, except that a timeout
///   holds no slot here either.
///
/// The hold (decision 4) assumes that abandoned work is still running at the
/// homeserver. A per-room download is a cheap indexed read for Synapse, at
/// most 0.34 s for 139 downloads in its logs from 2026-11-07 to 2026-11-10,
/// so that assumption does not hold for this request.
fn backup_outcome(result: &Result<Result<(), matrix_sdk::Error>, Elapsed>) -> Outcome {
    match result {
        Ok(Ok(())) => Outcome::Local,
        Err(_) => Outcome::Overloaded,
        Ok(Err(error)) => match error.outcome() {
            Outcome::TimedOut => Outcome::Overloaded,
            outcome => outcome,
        },
    }
}

/// One room's key-backup download for a sweep, or `None` if the sweep is
/// cancelled first (see [`unless_cancelled`]).
async fn download_for_sweep(
    client: &Client,
    backup: BackupSource<'_>,
    room_id: &RoomId,
    cancel: Option<&CancellationToken>,
) -> Option<Result<Result<(), matrix_sdk::Error>, Elapsed>> {
    unless_cancelled(cancel, download_backup_keys(client, backup, room_id)).await
}

/// `future`'s output, or `None` if `cancel` fires first. Recover and manual
/// re-decryption end a sweep by cancelling its token at their own deadline,
/// which is shorter than [`BACKUP_DOWNLOAD_TIMEOUT`] for recover. A download in
/// flight must end there too, not run out its own bound. Dropping it is safe,
/// since the import is one transaction, and it records no outcome:
///
/// - Sent as [`Caller::Background`], it frees its permit's slot. If it was the
///   breaker's probe, the probe is dropped and the next waiter probes at once.
/// - Sent as [`Caller::Interactive`] (backups not yet enabled, or a sweep a
///   user is waiting on), it held no slot and reports nothing.
async fn unless_cancelled<F: Future>(
    cancel: Option<&CancellationToken>,
    future: F,
) -> Option<F::Output> {
    match cancel {
        Some(cancel) => tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            output = future => Some(output),
        },
        None => Some(future.await),
    }
}

/// Re-decrypt a set of pending rows, fetching each `Room` handle once. Per-row
/// failures are logged and skipped; this never returns an error.
///
/// When `backup` is set (the startup sweep after `recover()`), each room's
/// megolm keys are downloaded from the server-side key backup before decrypting:
/// `recover()` only imports the backup *decryption key*, not the room keys
/// themselves, so without this the sweep finds the rows but can't decrypt them.
/// Each download goes through the homeserver gate as the given caller.
async fn redecrypt_rows(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    rows: Vec<PendingUtd>,
    backup: Option<BackupSource<'_>>,
    index: Option<&IndexHandle>,
    cancel: Option<&CancellationToken>,
) -> RedecryptSummary {
    let mut summary = RedecryptSummary::default();
    let mut by_room: HashMap<String, Vec<PendingUtd>> = HashMap::new();
    for row in rows {
        by_room.entry(row.room_id.clone()).or_default().push(row);
    }

    for (room_id, rows) in by_room {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            summary.timed_out = true;
            break;
        }
        let Some(room) = room_handle(client, &room_id) else {
            tracing::warn!(%account_id, room_id, "no room handle; skipping re-decryption for room");
            continue;
        };
        if let Some(backup) = backup {
            let Some(result) = download_for_sweep(client, backup, room.room_id(), cancel).await
            else {
                // This room's rows were not attempted, so they stay unmarked
                // for the next sweep.
                summary.timed_out = true;
                break;
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => {
                    tracing::warn!(%account_id, room_id, error = %err, "failed to download room keys from backup; rows may stay UTD");
                }
                Err(_elapsed) => {
                    tracing::warn!(%account_id, room_id, timeout_secs = BACKUP_DOWNLOAD_TIMEOUT.as_secs(),
                        "room key backup download timed out; rows may stay UTD");
                }
            }
        }
        for row in &rows {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                summary.timed_out = true;
                break;
            }
            summary.attempted += 1;
            summary.attempted_event_ids.push(row.event_id.clone());
            if redecrypt_one(&room, store, account_id, row, index).await {
                summary.decrypted += 1;
            }
        }
        if summary.timed_out {
            break;
        }
    }
    summary
}

/// Re-decrypt a single stored UTD and back-fill it on success. Every failure
/// path — a malformed envelope, a still-missing key, a write error — is logged
/// and swallowed.
async fn redecrypt_one(
    room: &Room,
    store: &Store,
    account_id: Uuid,
    row: &PendingUtd,
    index: Option<&IndexHandle>,
) -> bool {
    let raw: Raw<OriginalSyncRoomEncryptedEvent> =
        match serde_json::from_value(row.raw_event.clone()) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::warn!(
                    %account_id,
                    room_id = %room.room_id(),
                    event_id = row.event_id,
                    error = %err,
                    "malformed encrypted envelope; skipping re-decryption"
                );
                return false;
            }
        };

    let decrypted = match room.decrypt_event(&raw, None).await {
        Ok(ev) => ev,
        Err(err) => {
            tracing::debug!(
                %account_id,
                room_id = %room.room_id(),
                event_id = row.event_id,
                error = %err,
                "re-decryption failed; leaving as UTD"
            );
            return false;
        }
    };

    // `decrypt_event` returns a UTD `TimelineEvent` (not an error) when the key
    // still isn't available; only a genuine `Decrypted` kind yields content.
    let Some((content, event_type)) = extract_decrypted(&decrypted) else {
        return false;
    };

    // Plaintext-derived hot columns, now available post-decryption.
    let body_text = crate::meta::body_text(&content).map(str::to_owned);
    let relates_to = crate::meta::relates_to(&content);

    match store
        .update_decrypted_event(
            account_id,
            &row.event_id,
            &content,
            &event_type,
            body_text.as_deref(),
            relates_to.as_ref(),
        )
        .await
    {
        Ok(()) => {
            // debug, not info, to match the sibling failure log a few lines up:
            // a startup sweep can back-fill thousands of rows in one burst, and
            // the per-account count already logged in `sweep_pending_utds` is
            // the info-level signal that matters for routine visibility.
            tracing::debug!(
                %account_id,
                room_id = %room.room_id(),
                event_id = row.event_id,
                event_type = event_type.as_str(),
                "re-decrypted UTD"
            );
            // The body is now available, so re-indexing is owed. `update_decrypted_event`
            // wrote that obligation to `search_outbox` transactionally (this event plus
            // any relation target the now-decrypted body reveals); this is just the
            // best-effort wakeup hint (M9).
            if let Some(index) = index {
                index.notify();
            }
        }
        Err(err) => {
            tracing::warn!(
                %account_id,
                room_id = %room.room_id(),
                event_id = row.event_id,
                error = %err,
                "failed to back-fill re-decrypted event"
            );
            return false;
        }
    }

    // Record the crypto-provenance siblings now that we know how it decrypted —
    // a UTD had no EncryptionInfo at persist time. Best-effort; never fatal.
    if let Some(info) = decrypted_encryption_info(&decrypted) {
        let meta = crate::meta::crypto_meta(info);
        if let Err(err) = store
            .upsert_event_crypto(&meta.as_event_crypto(account_id, &row.event_id))
            .await
        {
            tracing::warn!(
                %account_id,
                room_id = %room.room_id(),
                event_id = row.event_id,
                error = %err,
                "failed to persist crypto sibling for re-decrypted event"
            );
        }
    }
    true
}

/// The pending row for an event that was just persisted, or `None` when it was
/// not stored as a UTD. Pure; the live ingest path uses it to hand
/// [`recheck_persisted_utd`] exactly what the table now holds.
pub(crate) fn pending_utd_for_persisted(
    event_type: &str,
    event_id: &str,
    room_id: &str,
    raw_event: &Value,
) -> Option<PendingUtd> {
    // Only a UTD with a readable session id is findable by the key-arrival
    // drain, so only that one can be missed by it.
    (event_type == "m.room.encrypted" && megolm_session_id(raw_event).is_some()).then(|| {
        PendingUtd {
            event_id: event_id.to_owned(),
            room_id: room_id.to_owned(),
            raw_event: raw_event.clone(),
        }
    })
}

/// Make one local decryption attempt for a UTD that was just persisted.
///
/// [`run`] drains the pending rows for a session when its key arrives. A row
/// written *after* that drain is invisible to it, and the SDK's decryption
/// result for that event was computed before the key landed -- so the row is
/// stored as a UTD although its key is already in the crypto store, and nothing
/// would look at it again until the next restart (#938). This closes that window
/// from the other side: whichever of "key arrives" and "row is written" finishes
/// second now sees the other.
///
/// Local only: no backup download, no homeserver traffic. A key that has not
/// arrived leaves the row pending for [`run`] to find when it does. Returns
/// whether the row was back-filled.
///
/// Safe on a duplicate delivery of an event that is already decrypted. The row
/// is built from the incoming envelope, so this can run for it, but
/// `upsert_event` never rewrites an existing row's `content` (its conflict
/// clause touches only `inline_media_urls`), and `update_decrypted_event` writes
/// only `WHERE content IS NULL`, so the second back-fill is a no-op and queues no
/// search-outbox entry. The cost is one local decrypt plus that guarded no-op
/// update. A lookup to skip them would add a database round trip to every new
/// UTD to save work on a rare re-delivery.
pub(crate) async fn recheck_persisted_utd(
    room: &Room,
    store: &Store,
    account_id: Uuid,
    row: &PendingUtd,
    index: Option<&IndexHandle>,
) -> bool {
    redecrypt_one(room, store, account_id, row, index).await
}

/// One batch of room keys from the SDK's arrival stream, as `(room, session)`
/// pairs, or `None` where the subscription lagged and dropped some.
type ArrivalBatch = Option<Vec<(OwnedRoomId, String)>>;

/// The room keys that reach the crypto store while one backfill page is being
/// decrypted and written (#947).
///
/// This is the backfill side of the race that [`recheck_persisted_utd`] closes
/// for live sync (#938). A key that lands after the SDK tried a paged event but
/// before its row is written is drained by [`run`] against a table that does not
/// have the row yet, so the row stays a UTD until the next restart. Live sync
/// re-checks every new UTD. Backfill cannot afford that: one page can store
/// dozens of rows whose keys will never arrive (the Steve-instance backlog),
/// and each attempt is an SDK decrypt and an SDK warning.
///
/// So backfill re-checks only the sessions that could have been missed. The
/// watch subscribes to the same arrival stream as [`run`] before the page is
/// decrypted, and after the page is written [`recheck_backfilled_sessions`]
/// reads what it buffered. A key that arrived before the subscription was in
/// the store when the SDK decrypted the event. A key that arrives after that
/// read is matched by [`run`], whose query then sees the written rows. Every
/// key in between was buffered here.
pub(crate) struct KeyArrivalWatch {
    /// `None` without an `OlmMachine`: no keys arrive, and nothing to watch.
    stream: Option<BoxStream<'static, ArrivalBatch>>,
}

impl KeyArrivalWatch {
    /// Subscribe to the arrival stream. Must be called before the page's events
    /// are decrypted.
    pub(crate) async fn start(client: &Client) -> Self {
        let stream = client
            .encryption()
            .room_keys_received_stream()
            .await
            .map(|stream| {
                stream
                    .map(|batch| {
                        batch.ok().map(|infos| {
                            infos
                                .into_iter()
                                .map(|info| (info.room_id, info.session_id))
                                .collect()
                        })
                    })
                    .boxed()
            });
        Self { stream }
    }

    /// Every batch buffered since [`start`](Self::start), without waiting for
    /// more.
    fn buffered(&mut self) -> Vec<ArrivalBatch> {
        let mut batches = Vec::new();
        if let Some(stream) = self.stream.as_mut() {
            // `now_or_never` is `None` once nothing more is buffered, and the
            // inner `None` is a closed stream.
            while let Some(Some(batch)) = stream.next().now_or_never() {
                batches.push(batch);
            }
        }
        batches
    }

    #[cfg(test)]
    fn from_stream(stream: BoxStream<'static, ArrivalBatch>) -> Self {
        Self {
            stream: Some(stream),
        }
    }
}

/// The sessions in `room_id` to re-check after a backfill page: those whose
/// keys arrived while it was being written, or every one of them if the
/// subscription lagged, since a dropped batch may have named any of them. Pure.
fn sessions_to_recheck<'a>(
    room_id: &RoomId,
    page_sessions: &'a BTreeSet<String>,
    arrivals: &[ArrivalBatch],
) -> Vec<&'a str> {
    if arrivals.iter().any(Option::is_none) {
        return page_sessions.iter().map(String::as_str).collect();
    }
    let arrived: BTreeSet<&str> = arrivals
        .iter()
        .flatten()
        .flatten()
        .filter(|(room, _)| room == room_id)
        .map(|(_, session)| session.as_str())
        .collect();
    page_sessions
        .iter()
        .map(String::as_str)
        .filter(|session| arrived.contains(session))
        .collect()
}

/// Drain the pending UTDs of every session in `page_sessions` whose key arrived
/// during the backfill page that `watch` covered (see [`KeyArrivalWatch`]).
/// Local only, like [`recheck_persisted_utd`]. Returns how many sessions were
/// re-checked.
pub(crate) async fn recheck_backfilled_sessions(
    room: &Room,
    store: &Store,
    account_id: Uuid,
    page_sessions: &BTreeSet<String>,
    mut watch: KeyArrivalWatch,
    index: Option<&IndexHandle>,
) -> usize {
    let arrivals = watch.buffered();
    let sessions = sessions_to_recheck(room.room_id(), page_sessions, &arrivals);
    if sessions.is_empty() {
        return 0;
    }
    tracing::debug!(
        %account_id,
        room_id = %room.room_id(),
        sessions = sessions.len(),
        lagged = arrivals.iter().any(Option::is_none),
        "re-checking backfilled UTDs whose keys arrived during the page"
    );
    let client = room.client();
    for session_id in &sessions {
        redecrypt_session(
            &client,
            store,
            account_id,
            room.room_id().as_str(),
            session_id,
            index,
        )
        .await;
    }
    sessions.len()
}

/// Parse a room ID string and look up its `Room` handle on the client.
fn room_handle(client: &Client, room_id: &str) -> Option<Room> {
    let room_id = RoomId::parse(room_id).ok()?;
    client.get_room(&room_id)
}

/// The key-backup download against a stand-in homeserver (ADR 0122): what a
/// download the SDK turns into a no-op does to the homeserver gate.
#[cfg(test)]
mod backup_gate_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Json, Router};
    use matrix_sdk::authentication::matrix::MatrixSession;
    use matrix_sdk::store::RoomLoadSettings;
    use matrix_sdk::{SessionMeta, SessionTokens};
    use serde_json::json;

    use super::*;
    use crate::homeserver_gate::{BreakerSnapshot, GROW_AFTER, INITIAL_LIMIT};

    /// A client with a session (so it has a crypto store) but no backup keys,
    /// against a homeserver that counts key-backup requests.
    pub(super) async fn client_without_backup_keys(backup_requests: Arc<AtomicUsize>) -> Client {
        let app = Router::new().fallback(move |uri: axum::http::Uri| {
            if uri.path().contains("/room_keys/") {
                backup_requests.fetch_add(1, Ordering::SeqCst);
            }
            async { Json(json!({})) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::builder()
            .homeserver_url(format!("http://{address}"))
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: "@backup:localhost".parse().unwrap(),
                        device_id: "BACKUPDEV".into(),
                    },
                    tokens: SessionTokens {
                        access_token: "backup-token".into(),
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        client
    }

    fn background(gate: &Arc<HomeserverGate>) -> BackupSource<'_> {
        BackupSource {
            gate,
            caller: Caller::Background,
        }
    }

    fn room() -> &'static RoomId {
        <&RoomId>::try_from("!room:localhost").unwrap()
    }

    /// Ten no-op downloads used to read as ten fast successes and raise the
    /// shared limit without contacting the homeserver.
    #[tokio::test]
    async fn backup_no_ops_never_grow_the_gate() {
        let requests = Arc::new(AtomicUsize::new(0));
        let client = client_without_backup_keys(Arc::clone(&requests)).await;
        let gate = HomeserverGate::for_test();
        for _ in 0..GROW_AFTER * 2 {
            download_backup_keys(&client, background(&gate), room())
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "no backup request was sent"
        );
        assert_eq!(gate.snapshot().limit, INITIAL_LIMIT);
    }

    /// A no-op download used to become the breaker's probe and close it at a
    /// limit of 1, resuming background traffic on a false recovery signal.
    #[tokio::test]
    async fn a_backup_no_op_neither_waits_on_nor_closes_an_open_breaker() {
        let requests = Arc::new(AtomicUsize::new(0));
        let client = client_without_backup_keys(Arc::clone(&requests)).await;
        let gate = HomeserverGate::for_test();
        gate.open_for_test();
        tokio::time::timeout(
            Duration::from_secs(5),
            download_backup_keys(&client, background(&gate), room()),
        )
        .await
        .expect("a download that cannot be sent waited for the gate")
        .unwrap()
        .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        let snapshot = gate.snapshot();
        assert!(
            matches!(snapshot.breaker, BreakerSnapshot::Open { .. }) && snapshot.limit == 0,
            "{snapshot:?}"
        );
    }

    /// Any request bound, for the permits these tests take directly.
    const T: Duration = Duration::from_secs(30);

    /// A download that never returns, as when the key import stalls locally.
    fn stalled() -> impl Future<Output = Result<(), matrix_sdk::Error>> {
        std::future::pending()
    }

    /// #1005: a download that ran out its bound held its slot for
    /// `TIMEOUT_HOLD`. The limit had already halved to 1, so the held slot shut
    /// the gate, and every other background request waited 15 minutes on a
    /// healthy homeserver. The download still halves the limit, but the next
    /// request is admitted at once.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_download_halves_the_limit_but_holds_no_slot() {
        let gate = HomeserverGate::for_test();
        let started = tokio::time::Instant::now();
        let result = gated_backup_download(
            &gate,
            Caller::Background,
            BACKUP_DOWNLOAD_TIMEOUT,
            stalled(),
        )
        .await;
        assert!(result.is_err(), "the download is cut at its bound");
        assert_eq!(started.elapsed(), BACKUP_DOWNLOAD_TIMEOUT);

        let snapshot = gate.snapshot();
        assert_eq!(
            (snapshot.limit, snapshot.active, snapshot.held),
            (INITIAL_LIMIT / 2, 0, 0),
            "{snapshot:?}"
        );
        gate.try_acquire(T)
            .expect("the next background request is admitted at once")
            .finish(Outcome::Success);
    }

    /// A stalled download that is the breaker's probe fails it, so the breaker
    /// reopens for its doubled cooldown, but holds no slot: the next probe is
    /// not pushed out to `TIMEOUT_HOLD`.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_download_fails_the_probe_without_holding_a_slot() {
        let gate = HomeserverGate::for_test();
        gate.open_for_test();
        let result = gated_backup_download(
            &gate,
            Caller::Background,
            BACKUP_DOWNLOAD_TIMEOUT,
            stalled(),
        )
        .await;
        assert!(result.is_err());
        let snapshot = gate.snapshot();
        assert_eq!(snapshot.held, 0, "{snapshot:?}");
        let BreakerSnapshot::Open { next_probe_at, .. } = snapshot.breaker else {
            panic!("a failed probe reopens the breaker: {snapshot:?}");
        };
        let wait = next_probe_at
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default();
        // Reopened from probing once: a doubled cooldown, not the hold, and
        // not a probe passed straight on.
        let cooldown = crate::homeserver_gate::COOLDOWN * 2;
        assert!(
            wait > cooldown - Duration::from_secs(5) && wait <= cooldown,
            "next probe in {wait:?}"
        );
    }

    /// A client with backups created and enabled, against `app` as its
    /// homeserver. `app` must answer backup creation (`POST
    /// /room_keys/version`) with a version.
    async fn client_with_backup(app: Router) -> Client {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = Client::builder()
            .homeserver_url(format!("http://{address}"))
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: "@backup:localhost".parse().unwrap(),
                        device_id: "BACKUPDEV".into(),
                    },
                    tokens: SessionTokens {
                        access_token: "backup-token".into(),
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        client.encryption().backups().create().await.unwrap();
        assert!(client.encryption().backups().are_enabled().await);
        client
    }

    /// A download refused by an overloaded homeserver is still load.
    #[tokio::test(start_paused = true)]
    async fn a_download_that_meets_a_refused_connection_still_halves_the_limit() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        let gate = HomeserverGate::for_test();
        let result =
            gated_backup_download(&gate, Caller::Background, BACKUP_DOWNLOAD_TIMEOUT, async {
                Err(refused.into())
            })
            .await;
        assert!(matches!(result, Ok(Err(_))));
        assert_eq!(gate.snapshot().limit, INITIAL_LIMIT / 2);
    }

    /// The SDK retries a 503 without limit under the client's default request
    /// config, so a homeserver that keeps answering 503 never surfaces as an
    /// error: the download only ever reaches its bound. That still has to
    /// count as load. Real SDK, real loopback server, a short bound in real
    /// time.
    #[tokio::test]
    async fn a_download_the_sdk_keeps_retrying_on_503_still_halves_the_limit() {
        let downloads = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&downloads);
        let app =
            Router::new().fallback(move |method: axum::http::Method, uri: axum::http::Uri| {
                let count = Arc::clone(&count);
                async move {
                    if method == axum::http::Method::GET && uri.path().contains("/room_keys/keys/")
                    {
                        count.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"errcode": "M_UNKNOWN", "error": "busy"})),
                        )
                    } else {
                        // Backup creation and anything else the SDK asks for.
                        (axum::http::StatusCode::OK, Json(json!({"version": "1"})))
                    }
                }
            });
        let client = client_with_backup(app).await;

        let gate = HomeserverGate::for_test();
        let result = gated_backup_download(
            &gate,
            Caller::Background,
            Duration::from_secs(3),
            client
                .encryption()
                .backups()
                .download_room_keys_for_room(room()),
        )
        .await;
        assert!(result.is_err(), "the SDK hid the 503s: {result:?}");
        assert!(downloads.load(Ordering::SeqCst) >= 1);
        let snapshot = gate.snapshot();
        assert_eq!(
            (snapshot.limit, snapshot.held),
            (INITIAL_LIMIT / 2, 0),
            "{snapshot:?}"
        );
    }

    /// Recover ends its sweep by cancelling it at 30 s. A download in flight
    /// then must end too, not run out its own 120 s bound with the identity
    /// lock held. Real SDK, against a homeserver that never answers the
    /// download, in real time.
    #[tokio::test]
    async fn cancelling_a_sweep_ends_the_download_in_flight() {
        let app = Router::new().fallback(
            |method: axum::http::Method, uri: axum::http::Uri| async move {
                if method == axum::http::Method::GET && uri.path().contains("/room_keys/keys/") {
                    std::future::pending::<()>().await;
                }
                Json(json!({"version": "1"}))
            },
        );
        let client = client_with_backup(app).await;
        let gate = HomeserverGate::for_test();
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            canceller.cancel();
        });
        let started = std::time::Instant::now();
        let result = download_for_sweep(&client, background(&gate), room(), Some(&cancel)).await;
        assert!(result.is_none(), "the sweep's cancellation won: {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let snapshot = gate.snapshot();
        assert_eq!(
            (snapshot.limit, snapshot.active, snapshot.held),
            (INITIAL_LIMIT, 0, 0),
            "a cancelled download teaches the gate nothing: {snapshot:?}"
        );
    }

    /// Cancel `cancel` after `after`, from another task.
    fn cancel_after(cancel: &CancellationToken, after: Duration) {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
    }

    /// A stalled download that is the breaker's probe and is cancelled neither
    /// passes nor fails the probe: the breaker stays open, and the next waiter
    /// probes at once instead of after another cooldown.
    #[tokio::test(start_paused = true)]
    async fn cancelling_the_probe_download_lets_the_next_request_probe() {
        let gate = HomeserverGate::for_test();
        gate.open_for_test();
        let cancel = CancellationToken::new();
        // Cancel once the cooldown is over and the download has become the
        // probe, noting the breaker's state just before.
        let canceller = {
            let (gate, cancel) = (gate.clone(), cancel.clone());
            tokio::spawn(async move {
                tokio::time::sleep(crate::homeserver_gate::COOLDOWN + Duration::from_secs(5)).await;
                let before = gate.snapshot().breaker;
                cancel.cancel();
                before
            })
        };
        let result = unless_cancelled(
            Some(&cancel),
            gated_backup_download(
                &gate,
                Caller::Background,
                BACKUP_DOWNLOAD_TIMEOUT,
                stalled(),
            ),
        )
        .await;
        assert!(result.is_none(), "the cancellation won: {result:?}");
        let before = canceller.await.expect("canceller task");
        assert!(
            matches!(before, BreakerSnapshot::Probing { .. }),
            "the download was the probe when cancelled: {before:?}"
        );

        let snapshot = gate.snapshot();
        assert_eq!((snapshot.active, snapshot.held), (0, 0), "{snapshot:?}");
        let BreakerSnapshot::Open { next_probe_at, .. } = snapshot.breaker else {
            panic!("a dropped probe leaves the breaker open: {snapshot:?}");
        };
        let wait = next_probe_at
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default();
        assert!(wait < Duration::from_secs(1), "next probe in {wait:?}");
        gate.try_acquire(T)
            .expect("the next request is admitted as the probe")
            .finish(Outcome::Success);
        assert!(
            matches!(gate.snapshot().breaker, BreakerSnapshot::Closed),
            "{:?}",
            gate.snapshot()
        );
    }

    /// An interactive download takes no permit, so a cancelled one leaves the
    /// gate exactly as it was: no halving, no slot, no breaker change.
    #[tokio::test(start_paused = true)]
    async fn cancelling_an_interactive_download_reports_nothing() {
        let gate = HomeserverGate::for_test();
        let cancel = CancellationToken::new();
        cancel_after(&cancel, Duration::from_secs(5));
        let result = unless_cancelled(
            Some(&cancel),
            gated_backup_download(
                &gate,
                Caller::Interactive,
                BACKUP_DOWNLOAD_TIMEOUT,
                stalled(),
            ),
        )
        .await;
        assert!(result.is_none(), "the cancellation won: {result:?}");
        let snapshot = gate.snapshot();
        assert_eq!(
            (snapshot.limit, snapshot.active, snapshot.held),
            (INITIAL_LIMIT, 0, 0),
            "{snapshot:?}"
        );
        assert!(
            matches!(snapshot.breaker, BreakerSnapshot::Closed),
            "{snapshot:?}"
        );
    }
}

/// [`KeyArrivalWatch`] against a real crypto store: a room key imported after
/// the watch starts is what the backfill re-check sees (#947).
#[cfg(test)]
mod key_arrival_tests {
    use std::sync::atomic::AtomicUsize;

    use matrix_sdk_base::crypto::encrypt_room_key_export;
    use matrix_sdk_base::crypto::olm::{InboundGroupSession, SenderData};
    use matrix_sdk_base::crypto::types::EventEncryptionAlgorithm;
    use matrix_sdk_base::crypto::vodozemac::megolm::{GroupSession, SessionConfig};
    use matrix_sdk_base::crypto::vodozemac::{
        Curve25519PublicKey, Curve25519SecretKey, Ed25519SecretKey,
    };

    use super::*;

    const PASSPHRASE: &str = "key-arrival-test";

    /// A room-key export file holding one new megolm session for `room_id`,
    /// and that session's id.
    async fn room_key_export(room_id: &RoomId) -> (std::path::PathBuf, String) {
        let outbound = GroupSession::new(SessionConfig::version_1());
        let session = InboundGroupSession::new(
            Curve25519PublicKey::from(&Curve25519SecretKey::new()),
            Ed25519SecretKey::new().public_key(),
            room_id,
            &outbound.session_key(),
            SenderData::unknown(),
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
            false,
        )
        .expect("inbound session");
        let export =
            encrypt_room_key_export(&[session.export().await], PASSPHRASE, 1).expect("key export");
        let path = std::env::temp_dir().join(format!("axon-key-arrival-{}.txt", Uuid::new_v4()));
        std::fs::write(&path, export).expect("write key export");
        (path, outbound.session_id())
    }

    async fn import(client: &Client, path: std::path::PathBuf) {
        let result = client
            .encryption()
            .import_room_keys(path.clone(), PASSPHRASE)
            .await;
        let _ = std::fs::remove_file(&path);
        assert_eq!(result.expect("import").imported_count, 1);
    }

    /// The race in #947: the SDK has already tried the paged event, then its
    /// key lands before the row is written. The watch, started before the
    /// attempt, must hold that key so the page's session is re-checked.
    #[tokio::test]
    async fn a_key_imported_during_the_page_is_rechecked() {
        let client =
            backup_gate_tests::client_without_backup_keys(Arc::new(AtomicUsize::new(0))).await;
        let room = <&RoomId>::try_from("!paged:localhost").unwrap();
        let (path, session_id) = room_key_export(room).await;

        let mut watch = KeyArrivalWatch::start(&client).await;
        import(&client, path).await;

        let page = BTreeSet::from([session_id.clone(), "never-arrives".to_owned()]);
        let arrivals = watch.buffered();
        assert_eq!(
            sessions_to_recheck(room, &page, &arrivals),
            vec![session_id.as_str()]
        );
    }

    /// A key already in the store when the watch starts was there for the
    /// SDK's decrypt attempt, so it is not re-checked.
    #[tokio::test]
    async fn a_key_imported_before_the_page_is_not_rechecked() {
        let client =
            backup_gate_tests::client_without_backup_keys(Arc::new(AtomicUsize::new(0))).await;
        let room = <&RoomId>::try_from("!paged:localhost").unwrap();
        let (path, session_id) = room_key_export(room).await;
        import(&client, path).await;

        let mut watch = KeyArrivalWatch::start(&client).await;
        let page = BTreeSet::from([session_id]);
        assert!(sessions_to_recheck(room, &page, &watch.buffered()).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::deserialized_responses::{DecryptedRoomEvent, UnableToDecryptInfo};
    use matrix_sdk::ruma::events::AnySyncTimelineEvent;
    use serde_json::json;

    const DECRYPTED_MESSAGE: &str =
        include_str!("../../../tests/fixtures/decryption/decrypted_message_event.json");
    const UTD_ENCRYPTED: &str =
        include_str!("../../../tests/fixtures/decryption/utd_encrypted_event.json");
    const MALFORMED: &str =
        include_str!("../../../tests/fixtures/decryption/malformed_encrypted_event.json");

    #[test]
    fn extracts_content_and_type_from_decrypted_event() {
        let raw: Raw<AnyTimelineEvent> = serde_json::from_str(DECRYPTED_MESSAGE).expect("fixture");
        let (content, event_type) = extract_content_and_type(&raw).expect("decrypted fields");
        assert_eq!(event_type, "m.room.message");
        assert_eq!(content["body"], "hello from a re-decrypted message");
    }

    #[test]
    fn malformed_event_yields_no_content() {
        let raw: Raw<AnyTimelineEvent> = serde_json::from_str(MALFORMED).expect("fixture");
        assert!(extract_content_and_type(&raw).is_none());
    }

    #[test]
    fn reads_session_id_from_utd_envelope() {
        let envelope: Value = serde_json::from_str(UTD_ENCRYPTED).expect("fixture");
        assert_eq!(megolm_session_id(&envelope), Some("session-abc-123"));
    }

    #[test]
    fn missing_session_id_is_none() {
        let envelope: Value = serde_json::from_str(MALFORMED).expect("fixture");
        assert_eq!(megolm_session_id(&envelope), None);
    }

    #[test]
    fn non_string_session_id_is_none() {
        // A `content.session_id` that is present but the wrong JSON type must
        // not be coerced — the hot column is text.
        let envelope = json!({ "content": { "session_id": 42 } });
        assert_eq!(megolm_session_id(&envelope), None);
    }

    #[test]
    fn content_without_session_id_is_none() {
        // `content` present (so the first `get` succeeds) but no `session_id`.
        let envelope = json!({ "content": { "algorithm": "m.megolm.v1.aes-sha2" } });
        assert_eq!(megolm_session_id(&envelope), None);
    }

    #[test]
    fn extract_content_and_type_missing_content_is_none() {
        // `type` present but `content` absent — neither half alone is enough.
        let raw: Raw<AnyTimelineEvent> =
            serde_json::from_value(json!({ "type": "m.room.message" })).expect("raw");
        assert!(extract_content_and_type(&raw).is_none());
    }

    #[test]
    fn a_persisted_utd_becomes_a_pending_row_for_recheck() {
        let envelope: Value = serde_json::from_str(UTD_ENCRYPTED).expect("fixture");
        let row = pending_utd_for_persisted("m.room.encrypted", "$e", "!r:x", &envelope)
            .expect("a UTD with a session id is rechecked");
        assert_eq!(row.event_id, "$e");
        assert_eq!(row.room_id, "!r:x");
        assert_eq!(row.raw_event, envelope);
    }

    #[test]
    fn a_decrypted_or_sessionless_event_is_not_rechecked() {
        let envelope: Value = serde_json::from_str(UTD_ENCRYPTED).expect("fixture");
        // Decrypted by the SDK before dispatch: stored with its real type.
        assert!(pending_utd_for_persisted("m.room.message", "$e", "!r:x", &envelope).is_none());
        // The drain could never match this row either, so a recheck cannot help.
        let malformed: Value = serde_json::from_str(MALFORMED).expect("fixture");
        assert!(pending_utd_for_persisted("m.room.encrypted", "$e", "!r:x", &malformed).is_none());
    }

    #[test]
    fn extract_decrypted_returns_none_for_utd() {
        // A still-undecryptable event reaches us as a UTD `TimelineEvent`, not
        // an error; `extract_decrypted` must yield nothing so we leave the row.
        let raw: Raw<AnySyncTimelineEvent> = serde_json::from_str(UTD_ENCRYPTED).expect("utd raw");
        let utd_info: UnableToDecryptInfo =
            serde_json::from_value(json!({ "session_id": "session-abc-123" })).expect("utd info");
        let ev = TimelineEvent::from_utd(raw, utd_info);
        assert!(extract_decrypted(&ev).is_none());
    }

    #[test]
    fn extract_decrypted_reads_decrypted_event() {
        // The `Decrypted` arm pulls content + type out of the cleartext event.
        let event_json: Value = serde_json::from_str(DECRYPTED_MESSAGE).expect("fixture");
        let decrypted: DecryptedRoomEvent = serde_json::from_value(json!({
            "event": event_json,
            "encryption_info": {
                "sender": "@alice:example.org",
                "algorithm_info": {
                    "MegolmV1AesSha2": {
                        "curve25519_key": "Curve25519KeyBase64",
                        "sender_claimed_keys": {}
                    }
                },
                "verification_state": "Verified"
            }
        }))
        .expect("decrypted room event");
        let ev = TimelineEvent::from_decrypted(decrypted, None);
        let (content, event_type) = extract_decrypted(&ev).expect("decrypted fields");
        assert_eq!(event_type, "m.room.message");
        assert_eq!(content["body"], "hello from a re-decrypted message");
    }

    fn room_id(id: &str) -> OwnedRoomId {
        id.parse().expect("room id")
    }

    fn sessions(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn arrived(pairs: &[(&str, &str)]) -> ArrivalBatch {
        Some(
            pairs
                .iter()
                .map(|(room, session)| (room_id(room), (*session).to_owned()))
                .collect(),
        )
    }

    /// Only a session of this page, in this room, whose key arrived during it
    /// is re-checked. A page full of never-decryptable sessions costs nothing.
    #[test]
    fn a_backfilled_page_rechecks_only_sessions_whose_keys_arrived() {
        let room = room_id("!r:x");
        let page = sessions(&["s1", "s2", "s3"]);
        let arrivals = [
            arrived(&[("!r:x", "s2"), ("!other:x", "s3")]),
            arrived(&[("!r:x", "elsewhere")]),
        ];
        assert_eq!(sessions_to_recheck(&room, &page, &arrivals), vec!["s2"]);
        assert!(sessions_to_recheck(&room, &page, &[]).is_empty());
    }

    /// A lagged subscription may have dropped a batch naming any of the page's
    /// sessions, so all of them are re-checked.
    #[test]
    fn a_lagged_watch_rechecks_every_session_of_the_page() {
        let room = room_id("!r:x");
        let page = sessions(&["s1", "s2"]);
        let arrivals = [arrived(&[("!r:x", "s2")]), None];
        assert_eq!(
            sessions_to_recheck(&room, &page, &arrivals),
            vec!["s1", "s2"]
        );
    }

    /// The watch hands back what arrived and returns at once when nothing
    /// more is buffered: it must never hold up the backfill loop waiting for
    /// a key.
    #[tokio::test]
    async fn the_watch_reads_buffered_arrivals_without_waiting() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ArrivalBatch>();
        let mut watch = KeyArrivalWatch::from_stream(
            futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed(),
        );
        tx.send(arrived(&[("!r:x", "s1")])).unwrap();
        tx.send(None).unwrap();
        let batches = tokio::time::timeout(Duration::from_secs(5), async { watch.buffered() })
            .await
            .expect("the read waited for more keys");
        assert_eq!(batches, vec![arrived(&[("!r:x", "s1")]), None]);
        // Still open, nothing buffered: an empty read, not a wait.
        assert!(watch.buffered().is_empty());
        drop(tx);
        assert!(watch.buffered().is_empty());
    }

    #[test]
    fn timed_out_preserves_counts() {
        let summary = RedecryptSummary {
            selected: 22_521,
            attempted: 100,
            decrypted: 0,
            still_pending: 0,
            startup_marked: 0,
            timed_out: false,
            attempted_event_ids: Vec::new(),
        };
        let out = RedecryptSummary::timed_out(summary);
        assert!(out.timed_out);
        assert_eq!(out.selected, 22_521);
        assert_eq!(out.attempted, 100);
        assert_eq!(out.decrypted, 0);
        assert_eq!(out.still_pending, 22_521);
    }
}
