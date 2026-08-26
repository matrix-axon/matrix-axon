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

use std::collections::HashMap;

use axon_search::IndexHandle;
use axon_store::{PendingUtd, Store};
use futures_util::StreamExt;
use matrix_sdk::deserialized_responses::{EncryptionInfo, TimelineEvent, TimelineEventKind};
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::AnyTimelineEvent;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::RoomId;
use matrix_sdk::{Client, Room};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
/// `cancel` is checked per room so a recover/manual timeout can stop the sweep
/// and still return the partial summary (ADR 0098). The boot path races this
/// against the supervised task's cancel token.
pub(crate) async fn sweep_pending_utds(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    scope: SweepScope,
    index: Option<&IndexHandle>,
    cancel: &CancellationToken,
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
    let mut summary =
        redecrypt_rows(client, store, account_id, rows, true, index, Some(cancel)).await;
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
    redecrypt_rows(client, store, account_id, rows, false, index, None).await;
}

/// Re-decrypt a set of pending rows, fetching each `Room` handle once. Per-row
/// failures are logged and skipped; this never returns an error.
///
/// When `from_backup` is set (the startup sweep after `recover()`), each room's
/// megolm keys are downloaded from the server-side key backup before decrypting:
/// `recover()` only imports the backup *decryption key*, not the room keys
/// themselves, so without this the sweep finds the rows but can't decrypt them.
async fn redecrypt_rows(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    rows: Vec<PendingUtd>,
    from_backup: bool,
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
        if from_backup {
            if let Err(err) = client
                .encryption()
                .backups()
                .download_room_keys_for_room(room.room_id())
                .await
            {
                tracing::warn!(%account_id, room_id, error = %err, "failed to download room keys from backup; rows may stay UTD");
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

/// Parse a room ID string and look up its `Room` handle on the client.
fn room_handle(client: &Client, room_id: &str) -> Option<Room> {
    let room_id = RoomId::parse(room_id).ok()?;
    client.get_room(&room_id)
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
