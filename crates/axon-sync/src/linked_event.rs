//! Reading one linked event from the homeserver, for a Matrix-link card (ADR
//! 0138 section 3).
//!
//! The API route reads its store first and asks this reader only for an event
//! the store does not have. The reader asks the account's homeserver for
//! `GET /_matrix/client/v3/rooms/{roomId}/event/{eventId}`, decrypts the answer
//! the way backfill decrypts a paged event, and judges its redaction and any
//! bundled edit by the rules the store's timeline applies. Nothing it reads is
//! written to the store: timeline reads come straight from the store, and an
//! isolated old event would appear beyond a backfill gap.
//!
//! The request is a background caller of the homeserver's load gate (ADR
//! 0122): it runs only because a message is on screen, and no one asked for
//! it. So it waits for a permit, but only briefly, and at most
//! [`MAX_CONCURRENT`] run at once across the process, a cap of their own.

use std::sync::Arc;
use std::time::Duration;

use axon_store::{AccountState, Store};
use matrix_sdk::config::RequestConfig;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::ruma::api::client::room::get_room_event;
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::AnyTimelineEvent;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use matrix_sdk::{Client, HttpError, RoomState};
use serde_json::Value;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::homeserver_gate::{outcome_of, GateError, HomeserverGates, Outcome};
use crate::manager::ClientManager;

/// Card reads in flight at once, across the process. Separate from the media
/// proxy's download slots, so a card read never takes a download's place.
pub(crate) const MAX_CONCURRENT: usize = 4;
/// How long a read waits, in all, for one of the [`MAX_CONCURRENT`] slots and
/// then a gate permit, before answering `503`.
pub(crate) const PERMIT_WAIT: Duration = Duration::from_secs(5);
/// The one deadline on the homeserver request, from when the permit is held.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// One event as a card shows it: its latest valid edit applied and its
/// redaction masked, as the timeline would show it.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedEvent {
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    pub origin_ts: i64,
    pub event_type: String,
    /// Decrypted content. `None` when redacted or undecryptable.
    pub content: Option<Value>,
    pub body: Option<String>,
    pub relates_to: Option<Value>,
    pub redacted: bool,
    /// A valid edit the homeserver bundled was applied.
    pub edited: bool,
    /// The verdict ingestion would have recorded (`meta::sender_trust`), the
    /// applied edit's when there is one. `None` for an unencrypted event and
    /// for one that could not be decrypted.
    pub sender_trust: Option<String>,
}

/// Why a linked event could not be read. The adapter in `axon-server` maps
/// these onto `axon-api`'s HTTP-shaped error.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LinkedEventError {
    /// Nothing to show: the room is not one the account has joined or left,
    /// or the homeserver answered `404 M_NOT_FOUND` or `403 M_FORBIDDEN`. `404`.
    #[error("event not found")]
    NotFound,
    /// A malformed room or event id. `400`.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// No account exists for this id. `404`.
    #[error("no such account: {0}")]
    UnknownAccount(Uuid),
    /// The account is logged out or being deleted. `409`.
    #[error("account not active: {0}")]
    NotActive(Uuid),
    /// The account has no connected client right now. `503`.
    #[error("account not connected: {0}")]
    NotConnected(Uuid),
    /// No slot or gate permit within [`PERMIT_WAIT`]. `503`.
    #[error("too many linked-event reads in flight")]
    Busy,
    /// The homeserver did not answer within [`REQUEST_TIMEOUT`]. `504`.
    #[error("homeserver did not answer in time")]
    TimedOut,
    /// Any other homeserver failure. `502`.
    #[error("upstream homeserver error: {0}")]
    Upstream(String),
    /// A store failure. `500`.
    #[error("store error: {0}")]
    Store(String),
}

/// Reads linked events. Cheap to clone; clones share the process-wide cap.
#[derive(Clone)]
pub struct LinkedEventReader {
    store: Store,
    manager: ClientManager,
    gates: HomeserverGates,
    slots: Arc<Semaphore>,
}

impl LinkedEventReader {
    pub(crate) fn new(store: Store, manager: ClientManager, gates: HomeserverGates) -> Self {
        Self {
            store,
            manager,
            gates,
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        }
    }

    /// Read `event_id` in `room_id` from the account's homeserver.
    ///
    /// Logs each outcome, so an operator can tell a card outage (busy,
    /// not connected) from a homeserver problem (timed out, upstream): the
    /// expected answers at `debug`, the homeserver's failures at `warn`.
    pub async fn read(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<LinkedEvent, LinkedEventError> {
        let result = self.read_unlogged(account_id, room_id, event_id).await;
        match &result {
            Ok(event) if event.content.is_none() && !event.redacted => tracing::debug!(
                account_id = %account_id,
                room_id,
                event_id,
                "linked event: read from the homeserver, could not be decrypted"
            ),
            Ok(event) => tracing::debug!(
                account_id = %account_id,
                room_id,
                event_id,
                redacted = event.redacted,
                edited = event.edited,
                "linked event: read from the homeserver"
            ),
            Err(
                err @ (LinkedEventError::NotFound
                | LinkedEventError::Invalid(_)
                | LinkedEventError::UnknownAccount(_)
                | LinkedEventError::NotActive(_)
                | LinkedEventError::NotConnected(_)
                | LinkedEventError::Busy),
            ) => tracing::debug!(
                account_id = %account_id,
                room_id,
                event_id,
                error = %err,
                "linked event: not read"
            ),
            Err(err @ (LinkedEventError::TimedOut | LinkedEventError::Upstream(_))) => {
                tracing::warn!(
                    account_id = %account_id,
                    room_id,
                    event_id,
                    error = %err,
                    "linked event: the homeserver failed"
                )
            }
            Err(err @ LinkedEventError::Store(_)) => tracing::error!(
                account_id = %account_id,
                room_id,
                event_id,
                error = %err,
                "linked event: store error"
            ),
        }
        result
    }

    async fn read_unlogged(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<LinkedEvent, LinkedEventError> {
        let room_id = RoomId::parse(room_id)
            .map_err(|err| LinkedEventError::Invalid(format!("room id: {err}")))?;
        let event_id = EventId::parse(event_id)
            .map_err(|err| LinkedEventError::Invalid(format!("event id: {err}")))?;

        // The authoritative account state, not the cached client's: a client
        // can stay cached for a moment after logout flips the row.
        let account = self
            .store
            .get_account(account_id)
            .await
            .map_err(|err| LinkedEventError::Store(err.to_string()))?
            .ok_or(LinkedEventError::UnknownAccount(account_id))?;
        if account.state != AccountState::Active {
            return Err(LinkedEventError::NotActive(account_id));
        }
        // Never connects: a card read is not worth a cold connect.
        let client = self
            .manager
            .cached(account_id)
            .ok_or(LinkedEventError::NotConnected(account_id))?;
        // Only rooms the account has joined or left: a sender cannot make the
        // client probe a room the account was never in, and an invited room's
        // messages are not shown before the invite is accepted.
        let room = client
            .get_room(&room_id)
            .filter(|room| matches!(room.state(), RoomState::Joined | RoomState::Left))
            .ok_or(LinkedEventError::NotFound)?;

        let raw = {
            let deadline = tokio::time::Instant::now() + PERMIT_WAIT;
            let _slot = tokio::time::timeout_at(deadline, self.slots.acquire())
                .await
                .map_err(|_| LinkedEventError::Busy)?
                .expect("the linked-event semaphore is never closed");
            let gate = self.gates.for_client(&client);
            let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
            let permit = gate
                .acquire_briefly(REQUEST_TIMEOUT, wait)
                .await
                .ok_or(LinkedEventError::Busy)?;
            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                send_event_request(&client, room_id.clone(), event_id.clone()),
            )
            .await;
            permit.finish(outcome_of(&result));
            match result {
                Err(_elapsed) => return Err(LinkedEventError::TimedOut),
                Ok(Err(err)) => return Err(map_http_error(&err)),
                Ok(Ok(raw)) => raw,
            }
        };

        // Decryption runs after the permit and slot are returned. It is
        // mostly local, but for a missing session the SDK may start a
        // backup-key download: a homeserver request outside the gate permit
        // and the deadline above, and this await has no timeout of its own.
        // That is accepted (the SDK de-duplicates and backs off those
        // downloads, and backfill and live sync start the same ones far more
        // often), not overlooked.
        let server_json: Value = serde_json::from_str(raw.json().get())
            .map_err(|err| LinkedEventError::Upstream(format!("unreadable event: {err}")))?;
        let original = decrypted(crate::backfill::decrypt_paged_event(&room, raw).await);
        let replacement = match bundled_replacement(&server_json) {
            Some(raw) => Some(decrypted(
                crate::backfill::decrypt_paged_event(&room, raw).await,
            )),
            None => None,
        };
        assemble(room_id.as_str(), &server_json, original, replacement)
            .ok_or_else(|| LinkedEventError::Upstream("malformed event".to_owned()))
    }
}

/// Send the request with the SDK's retries off: a struggling homeserver gets
/// no second request from a card, and the client tries again on its own the
/// next time the message is rendered.
async fn send_event_request(
    client: &Client,
    room_id: OwnedRoomId,
    event_id: OwnedEventId,
) -> Result<Raw<AnyTimelineEvent>, HttpError> {
    let request = get_room_event::v3::Request::new(room_id, event_id);
    let response = client
        .send(request)
        .with_request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(REQUEST_TIMEOUT),
        )
        .await?;
    Ok(response.event)
}

/// Only the two answers that mean the account may not read the event are
/// "nothing to show": `404 M_NOT_FOUND`, and `403 M_FORBIDDEN`, which is how a
/// homeserver refuses an event the account may not read. Anything else,
/// including a `403` or `404` with another code (a proxy's refusal, a rate
/// limit), is transient, so a card is never missing for the session because
/// of it.
fn map_http_error(err: &HttpError) -> LinkedEventError {
    if err.outcome() == Outcome::TimedOut {
        return LinkedEventError::TimedOut;
    }
    let status = err
        .as_client_api_error()
        .map(|api| api.status_code.as_u16());
    match (status, err.client_api_error_kind()) {
        (Some(404), Some(ErrorKind::NotFound)) | (Some(403), Some(ErrorKind::Forbidden)) => {
            LinkedEventError::NotFound
        }
        _ => LinkedEventError::Upstream(err.to_string()),
    }
}

/// A decrypted event's JSON and its sender-trust verdict.
pub(crate) struct Decrypted {
    json: Value,
    sender_trust: Option<&'static str>,
}

fn decrypted(event: TimelineEvent) -> Decrypted {
    let json = serde_json::from_str(event.raw().json().get()).unwrap_or(Value::Null);
    let sender_trust = event
        .encryption_info()
        .map(|info| crate::meta::sender_trust(&info.verification_state));
    Decrypted { json, sender_trust }
}

/// The whole replacement event a homeserver implementing Matrix 1.7 (MSC3925)
/// bundles in `unsigned["m.relations"]["m.replace"]`. Read from the event as
/// the homeserver sent it, not from the decrypted copy.
fn bundled_replacement(server_json: &Value) -> Option<Raw<AnyTimelineEvent>> {
    let replacement = server_json
        .get("unsigned")?
        .get("m.relations")?
        .get("m.replace")?;
    // An older server bundles only `{event_id, origin_server_ts, sender}`.
    replacement.get("content")?;
    let raw = serde_json::value::to_raw_value(replacement).ok()?;
    Some(Raw::from_json(raw))
}

const ENCRYPTED: &str = "m.room.encrypted";

/// Whether the homeserver says the event is redacted.
fn is_redacted(server_json: &Value) -> bool {
    server_json
        .get("unsigned")
        .and_then(|unsigned| unsigned.get("redacted_because"))
        .is_some_and(|because| !because.is_null())
}

/// Build the card's view of the event, applying the rules the store's
/// timeline applies (`latest_edit_lateral` in `axon-store`): a redaction masks
/// content, body and any edit; an undecryptable event has no content; a
/// bundled replacement is applied only when it is a valid edit. `None` when
/// the event lacks an id, sender, type or timestamp.
pub(crate) fn assemble(
    room_id: &str,
    server_json: &Value,
    original: Decrypted,
    replacement: Option<Decrypted>,
) -> Option<LinkedEvent> {
    let json = &original.json;
    let event_id = json.get("event_id")?.as_str()?.to_owned();
    let sender = json.get("sender")?.as_str()?.to_owned();
    let event_type = json.get("type")?.as_str()?.to_owned();
    let origin_ts = json
        .get("origin_server_ts")?
        .as_u64()
        .map(|ts| i64::try_from(ts).unwrap_or(i64::MAX))?;
    let mut event = LinkedEvent {
        event_id,
        room_id: room_id.to_owned(),
        sender,
        origin_ts,
        event_type,
        content: None,
        body: None,
        relates_to: None,
        redacted: false,
        edited: false,
        sender_trust: None,
    };
    if is_redacted(server_json) {
        event.redacted = true;
        return Some(event);
    }
    if event.event_type == ENCRYPTED {
        return Some(event);
    }
    let content = json.get("content").cloned().unwrap_or(Value::Null);
    event.relates_to = crate::meta::relates_to(&content);
    event.sender_trust = original.sender_trust.map(str::to_owned);
    match replacement.filter(|r| is_valid_edit(json, &r.json)) {
        Some(edit) => {
            let new_content = edit.json["content"]["m.new_content"].clone();
            event.body = crate::meta::body_text(&new_content).map(str::to_owned);
            event.content = Some(new_content);
            event.edited = true;
            event.sender_trust = edit.sender_trust.map(str::to_owned);
        }
        None => {
            event.body = crate::meta::body_text(&content).map(str::to_owned);
            event.content = Some(content);
        }
    }
    Some(event)
}

/// The store's rules for a valid `m.replace` (`latest_edit_lateral`): the same
/// sender and decrypted type as the original, neither a state event, the
/// original not itself an edit, a replacement that names the original and
/// carries an `m.new_content` object of the original's `msgtype`, and a
/// replacement that is neither redacted nor undecryptable.
fn is_valid_edit(original: &Value, replacement: &Value) -> bool {
    let original_content = &original["content"];
    let relation = &replacement["content"]["m.relates_to"];
    let new_content = &replacement["content"]["m.new_content"];
    replacement["type"] != ENCRYPTED
        && replacement["type"] == original["type"]
        && replacement["sender"] == original["sender"]
        && original.get("state_key").is_none()
        && replacement.get("state_key").is_none()
        && original_content["m.relates_to"]["rel_type"] != "m.replace"
        && relation["rel_type"] == "m.replace"
        && relation["event_id"] == original["event_id"]
        && new_content.is_object()
        && new_content.get("msgtype") == original_content.get("msgtype")
        && !is_redacted(replacement)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(event_id: &str, body: &str) -> Value {
        json!({
            "type": "m.room.message",
            "event_id": event_id,
            "sender": "@alice:example.org",
            "origin_server_ts": 1_700_000_000_000u64,
            "room_id": "!room:example.org",
            "content": {"msgtype": "m.text", "body": body},
        })
    }

    fn edit_of(event_id: &str, body: &str) -> Value {
        json!({
            "type": "m.room.message",
            "event_id": "$edit",
            "sender": "@alice:example.org",
            "origin_server_ts": 1_700_000_100_000u64,
            "room_id": "!room:example.org",
            "content": {
                "msgtype": "m.text",
                "body": format!("* {body}"),
                "m.new_content": {"msgtype": "m.text", "body": body},
                "m.relates_to": {"rel_type": "m.replace", "event_id": event_id},
            },
        })
    }

    fn plain(json: Value) -> Decrypted {
        Decrypted {
            json,
            sender_trust: None,
        }
    }

    fn trusted(json: Value, trust: &'static str) -> Decrypted {
        Decrypted {
            json,
            sender_trust: Some(trust),
        }
    }

    #[test]
    fn a_plain_event_is_returned_as_it_is() {
        let json = message("$a", "hello");
        let event = assemble("!room:example.org", &json, plain(json.clone()), None).unwrap();
        assert_eq!(event.event_id, "$a");
        assert_eq!(event.sender, "@alice:example.org");
        assert_eq!(event.origin_ts, 1_700_000_000_000);
        assert_eq!(event.body.as_deref(), Some("hello"));
        assert_eq!(event.content, Some(json["content"].clone()));
        assert!(!event.redacted && !event.edited);
        assert_eq!(event.sender_trust, None);
    }

    #[test]
    fn a_decrypted_event_carries_its_trust_verdict() {
        let json = message("$a", "hello");
        let event = assemble(
            "!room:example.org",
            &json,
            trusted(json.clone(), "mismatched_sender"),
            None,
        )
        .unwrap();
        assert_eq!(event.sender_trust.as_deref(), Some("mismatched_sender"));
        assert_eq!(event.body.as_deref(), Some("hello"));
    }

    #[test]
    fn an_undecryptable_event_has_no_content_and_no_trust() {
        let json = json!({
            "type": "m.room.encrypted",
            "event_id": "$a",
            "sender": "@alice:example.org",
            "origin_server_ts": 1u64,
            "content": {"algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "x"},
        });
        let event = assemble("!room:example.org", &json, plain(json.clone()), None).unwrap();
        assert_eq!(event.event_type, "m.room.encrypted");
        assert_eq!(event.content, None);
        assert_eq!(event.body, None);
        assert_eq!(event.sender_trust, None);
        assert!(!event.redacted);
    }

    #[test]
    fn a_redaction_masks_content_and_any_edit() {
        let mut server = message("$a", "secret");
        server["unsigned"] = json!({
            "redacted_because": {"type": "m.room.redaction", "event_id": "$r"},
            "m.relations": {"m.replace": edit_of("$a", "also secret")},
        });
        let edit = edit_of("$a", "also secret");
        let event = assemble(
            "!room:example.org",
            &server,
            trusted(server.clone(), "verified"),
            Some(trusted(edit, "verified")),
        )
        .unwrap();
        assert!(event.redacted);
        assert!(!event.edited);
        assert_eq!(event.content, None);
        assert_eq!(event.body, None);
        assert_eq!(event.sender_trust, None);
    }

    #[test]
    fn a_valid_bundled_edit_replaces_content_and_trust() {
        let json = message("$a", "first");
        let event = assemble(
            "!room:example.org",
            &json,
            trusted(json.clone(), "verified"),
            Some(trusted(edit_of("$a", "second"), "verification_violation")),
        )
        .unwrap();
        assert!(event.edited);
        assert_eq!(event.body.as_deref(), Some("second"));
        assert_eq!(
            event.content,
            Some(json!({"msgtype": "m.text", "body": "second"}))
        );
        assert_eq!(
            event.sender_trust.as_deref(),
            Some("verification_violation")
        );
    }

    #[test]
    fn an_invalid_bundled_edit_is_ignored() {
        let original = message("$a", "first");
        let mut by_someone_else = edit_of("$a", "forged");
        by_someone_else["sender"] = json!("@mallory:example.org");
        let mut of_another_event = edit_of("$b", "other");
        of_another_event["content"]["m.relates_to"]["event_id"] = json!("$b");
        let mut other_msgtype = edit_of("$a", "notice");
        other_msgtype["content"]["m.new_content"]["msgtype"] = json!("m.notice");
        let mut no_new_content = edit_of("$a", "x");
        no_new_content["content"]
            .as_object_mut()
            .unwrap()
            .remove("m.new_content");
        let mut redacted = edit_of("$a", "x");
        redacted["unsigned"] = json!({"redacted_because": {"event_id": "$r"}});
        let mut undecryptable = edit_of("$a", "x");
        undecryptable["type"] = json!("m.room.encrypted");
        let mut other_type = edit_of("$a", "x");
        other_type["type"] = json!("m.sticker");
        for edit in [
            by_someone_else,
            of_another_event,
            other_msgtype,
            no_new_content,
            redacted,
            undecryptable,
            other_type,
        ] {
            let event = assemble(
                "!room:example.org",
                &original,
                trusted(original.clone(), "verified"),
                Some(trusted(edit.clone(), "unknown")),
            )
            .unwrap();
            assert!(!event.edited, "{edit}");
            assert_eq!(event.body.as_deref(), Some("first"), "{edit}");
            assert_eq!(event.sender_trust.as_deref(), Some("verified"), "{edit}");
        }
    }

    #[test]
    fn an_edit_of_an_edit_is_not_applied() {
        let mut original = edit_of("$z", "an edit");
        original["event_id"] = json!("$a");
        let event = assemble(
            "!room:example.org",
            &original,
            plain(original.clone()),
            Some(plain(edit_of("$a", "again"))),
        )
        .unwrap();
        assert!(!event.edited);
    }

    #[test]
    fn only_a_whole_bundled_replacement_is_read() {
        let mut server = message("$a", "first");
        assert!(bundled_replacement(&server).is_none());
        server["unsigned"] = json!({"m.relations": {"m.replace": {
            "event_id": "$edit", "origin_server_ts": 1u64, "sender": "@alice:example.org"
        }}});
        assert!(bundled_replacement(&server).is_none());
        server["unsigned"]["m.relations"]["m.replace"] = edit_of("$a", "second");
        let raw = bundled_replacement(&server).expect("a whole replacement");
        let json: Value = serde_json::from_str(raw.json().get()).unwrap();
        assert_eq!(json["event_id"], "$edit");
    }

    #[test]
    fn an_event_missing_its_identity_is_malformed() {
        let mut json = message("$a", "x");
        json.as_object_mut().unwrap().remove("sender");
        assert!(assemble("!room:example.org", &json, plain(json.clone()), None).is_none());
    }

    mod homeserver {
        use std::net::SocketAddr;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::{Json, Router};

        use super::*;

        const EVENT: &str = "/_matrix/client/v3/rooms/{room_id}/event/{event_id}";

        async fn serve(router: Router) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            addr
        }

        async fn client_for(addr: SocketAddr) -> Client {
            use matrix_sdk::authentication::matrix::MatrixSession;
            use matrix_sdk::store::RoomLoadSettings;
            use matrix_sdk::{SessionMeta, SessionTokens};

            let client = Client::builder()
                .homeserver_url(format!("http://{addr}"))
                .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
                .build()
                .await
                .expect("client against the stand-in homeserver");
            client
                .matrix_auth()
                .restore_session(
                    MatrixSession {
                        meta: SessionMeta {
                            user_id: matrix_sdk::ruma::UserId::parse("@reader:localhost")
                                .expect("test user id"),
                            device_id: "READERDEV".into(),
                        },
                        tokens: SessionTokens {
                            access_token: "reader-token".to_owned(),
                            refresh_token: None,
                        },
                    },
                    RoomLoadSettings::default(),
                )
                .await
                .expect("restore a session so the request is sent");
            client
        }

        /// Answer every request with `status` and `body`, counting requests.
        fn answering(status: StatusCode, body: Value, hits: Arc<AtomicUsize>) -> Router {
            Router::new().route(
                EVENT,
                get(move || {
                    let body = body.clone();
                    let hits = Arc::clone(&hits);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (status, Json(body))
                    }
                }),
            )
        }

        async fn fetch(router: Router) -> Result<Raw<AnyTimelineEvent>, LinkedEventError> {
            let client = client_for(serve(router).await).await;
            send_event_request(
                &client,
                RoomId::parse("!room:localhost").unwrap(),
                EventId::parse("$event").unwrap(),
            )
            .await
            .map_err(|err| map_http_error(&err))
        }

        async fn error_for(status: StatusCode, body: Value) -> (LinkedEventError, usize) {
            let hits = Arc::new(AtomicUsize::new(0));
            let err = fetch(answering(status, body, Arc::clone(&hits)))
                .await
                .expect_err("the stand-in only fails");
            (err, hits.load(Ordering::SeqCst))
        }

        #[tokio::test]
        async fn an_event_the_homeserver_returns_is_read() {
            let hits = Arc::new(AtomicUsize::new(0));
            let raw = fetch(answering(
                StatusCode::OK,
                message("$event", "hello"),
                Arc::clone(&hits),
            ))
            .await
            .expect("the event");
            let json: Value = serde_json::from_str(raw.json().get()).unwrap();
            assert_eq!(json["content"]["body"], "hello");
        }

        #[tokio::test]
        async fn not_found_and_forbidden_mean_nothing_to_show() {
            let (err, _) = error_for(
                StatusCode::NOT_FOUND,
                json!({"errcode": "M_NOT_FOUND", "error": "Event not found."}),
            )
            .await;
            assert_eq!(err, LinkedEventError::NotFound);
            let (err, _) = error_for(
                StatusCode::FORBIDDEN,
                json!({"errcode": "M_FORBIDDEN", "error": "User not in room"}),
            )
            .await;
            assert_eq!(err, LinkedEventError::NotFound);
        }

        #[tokio::test]
        async fn any_other_refusal_is_transient() {
            for (status, body) in [
                (
                    StatusCode::FORBIDDEN,
                    json!({"errcode": "M_LIMIT_EXCEEDED", "error": "slow down"}),
                ),
                (
                    StatusCode::NOT_FOUND,
                    json!({"errcode": "M_UNRECOGNIZED", "error": "no such endpoint"}),
                ),
                (StatusCode::FORBIDDEN, json!({"not": "a matrix error"})),
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({"errcode": "M_LIMIT_EXCEEDED", "error": "slow down"}),
                ),
            ] {
                let (err, _) = error_for(status, body.clone()).await;
                assert!(
                    matches!(err, LinkedEventError::Upstream(_)),
                    "{status} {body}: {err:?}"
                );
            }
        }

        #[tokio::test]
        async fn a_server_error_is_not_retried() {
            let (err, hits) = error_for(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"errcode": "M_UNKNOWN", "error": "overloaded"}),
            )
            .await;
            assert!(matches!(err, LinkedEventError::Upstream(_)), "{err:?}");
            assert_eq!(hits, 1, "the SDK's retries are off");
        }
    }
}
