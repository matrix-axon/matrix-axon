//! Single-event endpoint, its relation aggregations (M8), and its per-event
//! verification bundle.

use std::collections::BTreeMap;
use std::sync::Arc;

use axon_store::Store;
use axum::extract::State;
use uuid::Uuid;

use crate::dto::{EventDto, LinkedEventDto, ReactionDto, VerificationBundleDto};
use crate::extract::Path;
use crate::linked_events::{KeptMedia, LinkedEventSource};
use crate::response::{ApiError, ApiResponse};
use crate::trust::SenderTrustService;

/// Get an event
///
/// Read a single event by `(account_id, event_id)`. Redacted events come back
/// masked (`content`/`body` null, `redacted` true); an unknown id is a 404.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/events/{event_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("event_id" = String, Path, description = "Matrix event id"),
    ),
    responses(
        (status = 200, description = "The event", body = ApiResponse<EventDto>),
        (status = 404, description = "No such event", body = crate::response::ErrorResponse),
    ),
    tag = "events",
)]
pub async fn get_event(
    State(store): State<Store>,
    Path((account_id, event_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<EventDto>, ApiError> {
    let row = store
        .get_event(account_id, &event_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("event {event_id} not found")))?;
    Ok(ApiResponse::new(EventDto::from_row(account_id, row)))
}

/// Get a linked event
///
/// Read one event in one room for a Matrix-link card (ADR 0138 section 3).
/// An event stored for this account and this room comes from the store, as
/// the timeline shows it; a stored event in another room is a 404, since event
/// ids are unique and the link is wrong. Otherwise, for a room the account has
/// joined or left, the account's homeserver is asked, and its answer is
/// decrypted, judged and returned without being stored; its media fields are
/// kept for an hour, so the media routes can serve its image when the request
/// names this event (`room_id` and `event_id`). Any other room, and a
/// homeserver `404 M_NOT_FOUND` or `403 M_FORBIDDEN`, is a 404. The homeserver
/// read waits for capacity at most a few seconds before answering 503; a 502,
/// 503 or 504 is transient.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
        ("event_id" = String, Path, description = "Matrix event id"),
    ),
    responses(
        (status = 200, description = "The event", body = ApiResponse<LinkedEventDto>),
        (status = 400, description = "Malformed room or event id", body = crate::response::ErrorResponse),
        (status = 404, description = "No such account, or nothing this account can show for the event", body = crate::response::ErrorResponse),
        (status = 409, description = "The account is not active", body = crate::response::ErrorResponse),
        (status = 502, description = "The homeserver failed. Transient", body = crate::response::ErrorResponse),
        (status = 503, description = "The account is not connected, or no capacity for the read. Transient", body = crate::response::ErrorResponse),
        (status = 504, description = "The homeserver did not answer in time. Transient", body = crate::response::ErrorResponse),
    ),
    tag = "events",
)]
pub async fn get_linked_event(
    State(store): State<Store>,
    State(source): State<Arc<dyn LinkedEventSource>>,
    State(kept): State<Arc<KeptMedia>>,
    Path((account_id, room_id, event_id)): Path<(Uuid, String, String)>,
) -> Result<ApiResponse<LinkedEventDto>, ApiError> {
    if let Some(row) = store.get_event(account_id, &event_id).await? {
        if row.room_id != room_id {
            return Err(ApiError::not_found(format!(
                "event {event_id} not found in room {room_id}"
            )));
        }
        return Ok(ApiResponse::new(LinkedEventDto::from_row(row)));
    }
    let mut event = source.read(account_id, &room_id, &event_id).await?;
    event.room_id = room_id;
    // Keeping nothing also drops what an earlier read kept, so an edit that
    // removed the image, or a redaction, stops it being served.
    kept.keep(
        account_id,
        &event.room_id,
        &event.event_id,
        event.content.as_ref().unwrap_or(&serde_json::Value::Null),
    );
    Ok(ApiResponse::new(LinkedEventDto::from_fetched(event)))
}

/// Get an event's verification bundle
///
/// Read the per-event verification bundle (M7c): the durable at-decrypt sender-
/// trust snapshot plus live cross-signing evidence for the sender's device and
/// identity. An unknown account/event is a 404; a logged-out account is a 409
/// (no live client to read evidence from). Gated by the bearer-token auth layer
/// like every `/v1/` route.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/events/{event_id}/verification",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("event_id" = String, Path, description = "Matrix event id"),
    ),
    responses(
        (status = 200, description = "The event's verification bundle", body = ApiResponse<VerificationBundleDto>),
        (status = 404, description = "No such account or event", body = crate::response::ErrorResponse),
        (status = 409, description = "The account is not active", body = crate::response::ErrorResponse),
        (status = 502, description = "Upstream error reading the sender's keys", body = crate::response::ErrorResponse),
    ),
    tag = "events",
)]
pub async fn get_verification_bundle(
    State(trust): State<Arc<dyn SenderTrustService>>,
    Path((account_id, event_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<VerificationBundleDto>, ApiError> {
    let bundle = trust.bundle(account_id, &event_id).await?;
    Ok(ApiResponse::new(VerificationBundleDto::from_bundle(
        event_id, bundle,
    )))
}

/// Get an event's reactions
///
/// Per-emoji reaction tally for an event (M8), resolved over the event's reactions
/// on disk regardless of where the original message falls in the timeline window —
/// the fix for the dropped-reaction bug (issue #22). The body is a JSON object
/// keyed by emoji:
/// `{ "👍": { "count": 2, "me": true, "senders": [...], "my_event_ids": [...] } }`.
/// An optional `shortcode` names custom MXC reactions when supplied by the sender.
/// `my_event_ids` carries the account user's own reaction event ids for the key —
/// the ids a client redacts to withdraw the reaction, since the collapsed
/// timeline no longer carries the raw `m.reaction` rows. An event with no
/// reactions (or an unknown event) yields an empty object (`{ "data": {} }`), not
/// a 404 — an absent event and one with no reactions are indistinguishable here
/// and empty is the natural answer. Against a pathological event, the store
/// hard-caps the tally at the oldest 1000 distinct `(sender, key)` pairs
/// (deterministically by `(origin_ts, event_id)`), so an event beyond that bound
/// reports those rather than literally every reaction.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/events/{event_id}/reactions",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("event_id" = String, Path, description = "Matrix event id"),
    ),
    responses(
        (status = 200, description = "Per-emoji reaction tallies, keyed by emoji", body = ApiResponse<BTreeMap<String, ReactionDto>>),
    ),
    tag = "events",
)]
pub async fn get_reactions(
    State(store): State<Store>,
    Path((account_id, event_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<BTreeMap<String, ReactionDto>>, ApiError> {
    let tallies = store.event_reactions(account_id, &event_id).await?;
    let map = tallies
        .into_iter()
        .map(|t| (t.key.clone(), ReactionDto::from(t)))
        .collect();
    Ok(ApiResponse::new(map))
}

/// Get an event's replies
///
/// Direct replies to an event (M8): events whose nested
/// `m.relates_to.m.in_reply_to.event_id` is this event and which carry no
/// `rel_type` (a plain reply, not a thread member). Oldest first;
/// redaction-masked. An event with no replies (or an unknown event) yields an
/// empty array, not a 404 — empty is the natural answer. The result is
/// hard-capped in the store against a pathological room.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/events/{event_id}/replies",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("event_id" = String, Path, description = "Matrix event id"),
    ),
    responses(
        (status = 200, description = "Direct replies, oldest first", body = ApiResponse<Vec<EventDto>>),
    ),
    tag = "events",
)]
pub async fn get_replies(
    State(store): State<Store>,
    Path((account_id, event_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<EventDto>>, ApiError> {
    let rows = store.event_replies(account_id, &event_id).await?;
    let replies = rows
        .into_iter()
        .map(|r| EventDto::from_row(account_id, r))
        .collect();
    Ok(ApiResponse::new(replies))
}

/// Get an event's edit history
///
/// The forensic edit history of an event (M8): every `m.replace` targeting it,
/// oldest first, *unfiltered* by the resolution rules — including edits from a
/// non-sender or an incompatible `msgtype` — for clients that want to show or
/// audit the edit trail. The collapsed timeline (and `EventDto.edited` /
/// `edit_count`) is the resolved view; this is the raw provenance. An event with
/// no edits (or an unknown event) yields an empty array, not a 404. The result is
/// hard-capped in the store against a pathological edit count.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/events/{event_id}/edits",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("event_id" = String, Path, description = "Matrix event id"),
    ),
    responses(
        (status = 200, description = "The forensic edit trail, oldest first", body = ApiResponse<Vec<EventDto>>),
    ),
    tag = "events",
)]
pub async fn get_edits(
    State(store): State<Store>,
    Path((account_id, event_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<EventDto>>, ApiError> {
    let rows = store.event_edits(account_id, &event_id).await?;
    let edits = rows
        .into_iter()
        .map(|r| EventDto::from_row(account_id, r))
        .collect();
    Ok(ApiResponse::new(edits))
}
