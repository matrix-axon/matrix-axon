//! Handwritten Axon `/v1/` wire types.
//!
//! These mirror the checked-in `openapi/openapi.json` contract (and the
//! `/v1/ws` envelope, which OpenAPI 3.1 cannot express). They are duplicated
//! here on purpose: the smoke harness is black-box and must not import any
//! `axon-*` crate, so drift between these types and the real API is caught by
//! the `live` profile (S2), never by a shared import.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Success envelope: every 2xx body is `{ "data": <T> }`.
#[derive(Debug, Serialize)]
pub struct ApiResponse<T> {
    pub data: T,
}

/// Account as exposed by `GET /v1/accounts` / `GET /v1/accounts/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct AccountDto {
    pub account_id: Uuid,
    pub user_id: String,
    pub homeserver_url: String,
    pub device_id: Option<String>,
    pub state: &'static str,
    pub verified: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
}

/// Room as exposed by `GET /v1/rooms`.
#[derive(Debug, Clone, Serialize)]
pub struct RoomDto {
    pub account_id: Uuid,
    pub account_user_id: String,
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub last_activity_ts: i64,
    pub last_event_id: Option<String>,
}

/// A single timeline event (timeline element and `/v1/ws` payload).
#[derive(Debug, Clone, Serialize)]
pub struct EventDto {
    pub account_id: Uuid,
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    pub state_key: Option<String>,
    /// Monotonic ingest order, `events.id` on a real axon (ADR 0089).
    ///
    /// The TUI's own `EventDto` declares this **without** `#[serde(default)]`
    /// on purpose — a defaulted `0` would freeze the read-receipt target on
    /// the first event forever, so it would rather fail to deserialize. That
    /// makes it required here too: a stub that omits it serves pages the TUI
    /// rejects wholesale, which renders an empty timeline and no error the
    /// scenarios can see (#190).
    pub arrival_order: i64,
    pub origin_ts: i64,
    #[serde(rename = "type")]
    pub event_type: String,
    pub content: Option<serde_json::Value>,
    pub body: Option<String>,
    pub relates_to: Option<serde_json::Value>,
    pub redacted: bool,
    pub redaction_event_id: Option<String>,
}

/// One page of a room timeline.
#[derive(Debug, Clone, Serialize)]
pub struct TimelinePage {
    pub events: Vec<EventDto>,
    pub next_cursor: Option<String>,
}

/// Result of a successful mutation.
#[derive(Debug, Clone, Serialize)]
pub struct SendResultDto {
    pub event_id: String,
}

/// `/v1/ws` frame envelope: `{ "type", "account_id", "payload" }`.
#[derive(Debug, Clone, Serialize)]
pub struct WsEnvelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub account_id: Uuid,
    pub payload: EventDto,
}

/// Body of `POST .../send`.
#[derive(Debug, Deserialize)]
pub struct SendRequest {
    pub body: String,
}

/// One entry in `GET .../rooms/{room_id}/threads`.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummaryDto {
    pub root_event_id: String,
    pub reply_count: i64,
}
