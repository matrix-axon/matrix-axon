//! The `/v1/ws` live-event WebSocket.
//!
//! A client opens one socket and receives every event the sync engine persists,
//! across all of this Axon's accounts, as it arrives, plus interactive
//! device-verification flow frames and allowlisted raw ephemeral events (ADR
//! 0056) forwarded without persistence. Each frame is a JSON envelope
//! `{ "type", "account_id", "payload" }` ([`WsEnvelope`]) — the same
//! `type`/`account_id`/`payload` shape used elsewhere on the wire. The `type`
//! tag discriminates the kind: `timeline.event` carries the read API's
//! [`EventDto`]; `verification.{requested,sas,done,cancelled}` carry a
//! [`VerificationFramePayload`] (SAS emoji/decimals, optional target device,
//! outcome); `ephemeral.passthrough` carries an allowlisted raw ephemeral
//! event (`m.typing`, `m.receipt`, …) verbatim (ADR 0056);
//! `unread_counts.changed` carries a room's SDK-derived unread counts
//! (issue #313, ADR 0070) — *not* built on the ADR 0056 ephemeral path, since
//! notification counts are per-room counters, not an ephemeral event.
//!
//! Delivery is **best-effort live tail**, not a replay: a client sees events
//! that arrive after it connects, and uses the HTTP read API for history. The
//! fan-out rides a [`tokio::sync::broadcast`] channel, so a client too slow to
//! keep up is told it lagged (and skips the backlog) rather than ever stalling
//! the sync engine. Writing a frame to the socket itself is bounded too (see
//! `WRITE_TIMEOUT`): a peer that stops draining its TCP receive buffer gets
//! disconnected rather than parking the connection's task forever (#290).
//!
//! Like every `/v1/` route the socket requires a valid bearer token (M7b), but
//! it can't ride the HTTP `require_bearer` layer: a browser can't set an
//! `Authorization` header on a WebSocket. So the handler reads the token itself
//! at upgrade time — from the `Authorization` header (non-browser clients like
//! the TUI) or a `bearer.<token>` entry in `Sec-WebSocket-Protocol` (browsers) —
//! and rejects with `401` before upgrading if it is missing or invalid. The
//! token-bearing subprotocol is **never echoed** in the 101 response, so the
//! secret doesn't land in response headers/logs; the server instead echoes the
//! benign [`WS_SUBPROTOCOL`] name when the client offered it, keeping the
//! handshake RFC 6455-compliant for browsers (see #238).
//!
//! A long-lived socket also re-checks its token on an interval (revocation
//! happens out-of-process, via the CLI, so there is no push signal) and closes
//! when the token is revoked — otherwise a revoked client would keep receiving
//! frames forever.

use std::sync::Arc;
use std::time::Duration;

use axon_core::{
    DeviceStateFrame, EphemeralFrame, LiveFrame, SenderTrustFrame, SyncStateFrame,
    UnreadCountsFrame, VerificationFrame, VerificationFrameKind,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::auth::{self, TokenVerifier};
use crate::dto::EventDto;
use crate::state::WsRevalidationInterval;

/// The `type` tag for a live timeline event frame. Namespaced so other frame
/// kinds (e.g. the `verification.*` frames below) extend the protocol without
/// colliding.
const TIMELINE_EVENT: &str = "timeline.event";

/// The benign WebSocket subprotocol the server negotiates when a browser client
/// offers it. It carries no credential: browsers offer `axon, bearer.<token>`
/// and the server echoes only `axon`, never the token-bearing entry. Echoing
/// *some* offered subprotocol is required so the 101 handshake is RFC 6455 §4.1
/// compliant (Chrome fails the connection otherwise); see #238 and ADR 0029.
const WS_SUBPROTOCOL: &str = "axon";

/// How long `pump` will wait for a frame write to drain before giving up on
/// the client. A live-tail reader that can't accept one frame in this long is
/// already indistinguishable from a dead peer; without this bound `send` can
/// park inside its `select!` branch indefinitely (a suspended laptop, a
/// NAT half-open connection, a deliberately slow reader), which also stalls
/// the periodic token-revalidation branch and lets a revoked token keep
/// receiving frames (issue #290).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The wire envelope for every `/v1/ws` frame: a `type` discriminant, the
/// `account_id` the frame pertains to, and a type-specific `payload`.
#[derive(Debug, Serialize)]
struct WsEnvelope<T> {
    #[serde(rename = "type")]
    kind: &'static str,
    account_id: Uuid,
    payload: T,
}

/// The wire payload for a `verification.*` frame: the live state of one SAS
/// verification flow. Fields that don't apply to a given stage are omitted
/// (e.g. `emoji`/`decimals` only on a `verification.sas` frame, `reason` only on
/// `verification.cancelled`).
#[derive(Debug, Serialize)]
struct VerificationFramePayload {
    flow_id: String,
    user_id: String,
    device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    emoji: Option<Vec<EmojiPair>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decimals: Option<[u16; 3]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// One SAS emoji: the symbol and its short English description.
#[derive(Debug, Serialize)]
struct EmojiPair {
    symbol: String,
    description: String,
}

/// The `type` tag for a sender-trust overlay frame (M7c).
const SENDER_TRUST_VIOLATION: &str = "sender_trust.violation";

/// The wire payload for a `sender_trust.violation` frame: the sender whose
/// current trust changed and the new violation state. Clients re-read the
/// timeline / verification bundle for that sender rather than diffing per-event.
#[derive(Debug, Serialize)]
struct SenderTrustFramePayload {
    user_id: String,
    verification_violation: bool,
}

impl From<SenderTrustFrame> for SenderTrustFramePayload {
    fn from(frame: SenderTrustFrame) -> Self {
        Self {
            user_id: frame.user_id,
            verification_violation: frame.verification_violation,
        }
    }
}

/// The `type` tag for a per-device state change frame (M12).
const DEVICE_STATE_CHANGED: &str = "device_state.changed";

/// The wire payload for a `device_state.changed` frame: the originating device,
/// the namespace, and the written entries (`null` = the key was deleted).
/// Receivers whose own device id matches `device_id` drop the frame (echo
/// suppression); on reconnect clients re-read the merged view over HTTP rather
/// than assuming the frames they missed.
#[derive(Debug, Serialize)]
struct DeviceStateFramePayload {
    device_id: Uuid,
    namespace: String,
    entries: serde_json::Map<String, serde_json::Value>,
    /// RFC 3339, matching the read API's timestamp shape.
    updated_at: String,
}

impl From<DeviceStateFrame> for DeviceStateFramePayload {
    fn from(frame: DeviceStateFrame) -> Self {
        Self {
            device_id: frame.device_id,
            namespace: frame.namespace,
            entries: frame
                .entries
                .into_iter()
                .map(|(key, value)| (key, value.unwrap_or(serde_json::Value::Null)))
                .collect(),
            updated_at: frame.updated_at.to_rfc3339(),
        }
    }
}

/// The `type` tag for a generic ephemeral passthrough frame (ADR 0056).
const EPHEMERAL_PASSTHROUGH: &str = "ephemeral.passthrough";

/// The wire payload for an `ephemeral.passthrough` frame: an allowlisted raw
/// ephemeral event forwarded verbatim. `room_id` is absent for account-scoped
/// signals (e.g. a future presence frame).
#[derive(Debug, Serialize)]
struct EphemeralFramePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    room_id: Option<String>,
    event_type: String,
    content: serde_json::Value,
}

impl From<EphemeralFrame> for EphemeralFramePayload {
    fn from(frame: EphemeralFrame) -> Self {
        Self {
            room_id: frame.room_id,
            event_type: frame.event_type,
            content: frame.content,
        }
    }
}

/// The `type` tag for an SDK-derived unread-counts frame (issue #313, ADR
/// 0070).
const UNREAD_COUNTS_CHANGED: &str = "unread_counts.changed";

/// The wire payload for an `unread_counts.changed` frame: a room's
/// SDK-derived notification/highlight counts, sourced from matrix-sdk's
/// read-receipt-based unread counters. See [`UnreadCountsFrame`].
#[derive(Debug, Serialize)]
struct UnreadCountsFramePayload {
    room_id: String,
    notification_count: u64,
    highlight_count: u64,
}

impl From<UnreadCountsFrame> for UnreadCountsFramePayload {
    fn from(frame: UnreadCountsFrame) -> Self {
        Self {
            room_id: frame.room_id,
            notification_count: frame.notification_count,
            highlight_count: frame.highlight_count,
        }
    }
}

/// The `type` tag for an account sync-readiness transition frame (ADR 0030,
/// issue #241).
const ACCOUNT_SYNC_STATE: &str = "account.sync_state";

/// The wire payload for an `account.sync_state` frame: the account's new
/// readiness value. See [`crate::dto::AccountDto::sync_state`] for what each
/// value means.
#[derive(Debug, Serialize)]
struct SyncStateFramePayload {
    sync_state: &'static str,
}

impl From<SyncStateFrame> for SyncStateFramePayload {
    fn from(frame: SyncStateFrame) -> Self {
        Self {
            sync_state: frame.sync_state,
        }
    }
}

/// The `type` tag for a verification frame of the given kind.
fn verification_type(kind: VerificationFrameKind) -> &'static str {
    match kind {
        VerificationFrameKind::Requested => "verification.requested",
        VerificationFrameKind::Sas => "verification.sas",
        VerificationFrameKind::Done => "verification.done",
        VerificationFrameKind::Cancelled => "verification.cancelled",
    }
}

impl From<VerificationFrame> for VerificationFramePayload {
    fn from(frame: VerificationFrame) -> Self {
        Self {
            flow_id: frame.flow_id,
            user_id: frame.target_user_id,
            device_id: frame.target_device_id,
            emoji: frame.emoji.map(|pairs| {
                pairs
                    .into_iter()
                    .map(|(symbol, description)| EmojiPair {
                        symbol,
                        description,
                    })
                    .collect()
            }),
            decimals: frame.decimals.map(|(a, b, c)| [a, b, c]),
            reason: frame.outcome,
        }
    }
}

/// `GET /v1/ws` — authenticate, then upgrade the connection and stream live
/// frames to the client.
pub async fn ws_handler(
    State(live): State<broadcast::Sender<LiveFrame>>,
    State(verifier): State<Arc<dyn TokenVerifier>>,
    State(WsRevalidationInterval(revalidation)): State<WsRevalidationInterval>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // Token via the Authorization header, or a `bearer.<token>` subprotocol for
    // browser clients that can't set headers on a socket.
    let Some(token) = auth::bearer_from_headers(&headers)
        .map(str::to_owned)
        .or_else(|| bearer_subprotocol(&headers))
    else {
        // The upgrade is an ordinary HTTP request until the 101, so the same
        // RFC 6750 `WWW-Authenticate` challenges as the HTTP gate apply here.
        return auth::missing_token_response("missing bearer token");
    };
    match verifier.verify(&token).await {
        Ok(true) => {}
        Ok(false) => return auth::invalid_token_response("invalid or revoked token"),
        Err(err) => return err.into_response(),
    }

    // Subscribe before the upgrade completes so no event that arrives during the
    // handshake is missed. We echo the benign `axon` subprotocol when the client
    // offered it (`protocols` selects a protocol only if the client requested it),
    // which keeps the 101 handshake RFC 6455-compliant for browsers. We never echo
    // the token-bearing `bearer.<token>` entry — that would place the secret in the
    // 101 response headers, where proxies and access logs may capture it. Header-auth
    // clients (the TUI) offer no subprotocols, so nothing is negotiated for them.
    let rx = live.subscribe();
    ws.protocols([WS_SUBPROTOCOL])
        .on_upgrade(move |socket| pump(socket, rx, verifier, token, revalidation))
}

/// Find a `bearer.<token>` entry in the `Sec-WebSocket-Protocol` header and
/// return the token. The credential is accepted here but never echoed back as
/// the negotiated subprotocol — the server negotiates the benign
/// [`WS_SUBPROTOCOL`] instead (see [`ws_handler`]).
fn bearer_subprotocol(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("sec-websocket-protocol")?.to_str().ok()?;
    raw.split(',')
        .map(str::trim)
        .find_map(|proto| proto.strip_prefix("bearer."))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
}

/// Serialize a [`LiveFrame`] to its `/v1/ws` envelope JSON. Each frame kind has
/// its own `type` tag and payload type, so the branches serialize separately.
fn encode_frame(frame: LiveFrame) -> Result<String, serde_json::Error> {
    match frame {
        LiveFrame::Timeline(event) => serde_json::to_string(&WsEnvelope {
            kind: TIMELINE_EVENT,
            account_id: event.account_id,
            payload: EventDto::from(event),
        }),
        LiveFrame::Verification(frame) => serde_json::to_string(&WsEnvelope {
            kind: verification_type(frame.kind),
            account_id: frame.account_id,
            payload: VerificationFramePayload::from(frame),
        }),
        LiveFrame::SenderTrustChanged(frame) => serde_json::to_string(&WsEnvelope {
            kind: SENDER_TRUST_VIOLATION,
            account_id: frame.account_id,
            payload: SenderTrustFramePayload::from(frame),
        }),
        LiveFrame::DeviceState(frame) => serde_json::to_string(&WsEnvelope {
            kind: DEVICE_STATE_CHANGED,
            account_id: frame.account_id,
            payload: DeviceStateFramePayload::from(frame),
        }),
        LiveFrame::Ephemeral(frame) => serde_json::to_string(&WsEnvelope {
            kind: EPHEMERAL_PASSTHROUGH,
            account_id: frame.account_id,
            payload: EphemeralFramePayload::from(frame),
        }),
        LiveFrame::UnreadCountsChanged(frame) => serde_json::to_string(&WsEnvelope {
            kind: UNREAD_COUNTS_CHANGED,
            account_id: frame.account_id,
            payload: UnreadCountsFramePayload::from(frame),
        }),
        LiveFrame::SyncStateChanged(frame) => serde_json::to_string(&WsEnvelope {
            kind: ACCOUNT_SYNC_STATE,
            account_id: frame.account_id,
            payload: SyncStateFramePayload::from(frame),
        }),
    }
}

/// The result of a [`send_with_timeout`] call.
enum SendOutcome {
    /// The peer's socket accepted the message.
    Delivered,
    /// The peer hung up.
    Closed,
    /// The peer didn't drain the write within [`WRITE_TIMEOUT`].
    TimedOut,
}

/// Send one message on `socket`, bounded by [`WRITE_TIMEOUT`]. Every send in
/// `pump` goes through this — including the best-effort revocation `Close`
/// notice — so a peer that stops draining its TCP receive buffer (suspended
/// laptop, half-open NAT, deliberately slow reader) can never park the task
/// indefinitely, which would otherwise also suspend the `select!`'s
/// revalidation branch and let a revoked token keep its stream open (#290).
async fn send_with_timeout(socket: &mut WebSocket, message: Message) -> SendOutcome {
    match tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => SendOutcome::Delivered,
        Ok(Err(_)) => SendOutcome::Closed,
        Err(_) => SendOutcome::TimedOut,
    }
}

/// Forward live frames to one connected client until either side hangs up or the
/// client's token is revoked.
///
/// `verifier` + `token` + `revalidation` drive a periodic token re-check:
/// revocation happens out-of-process (the `axon token revoke` CLI writes the DB),
/// so a live socket polls to notice it and closes when the token stops verifying.
async fn pump(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<LiveFrame>,
    verifier: Arc<dyn TokenVerifier>,
    token: String,
    revalidation: Duration,
) {
    let mut revalidate = tokio::time::interval(revalidation);
    // The interval's first tick is immediate; consume it — we just verified the
    // token at upgrade, so the first *re*-check should be one interval out.
    revalidate.tick().await;

    loop {
        tokio::select! {
            // Periodic token revalidation. A revoked token closes the socket; a
            // transient verifier error is logged but does not drop a live client.
            _ = revalidate.tick() => {
                match verifier.verify(&token).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::info!("websocket token revoked; closing socket");
                        // Best-effort notice: the socket is closing either way, so a
                        // timed-out or failed send changes nothing here.
                        let _ = send_with_timeout(&mut socket, Message::Close(None)).await;
                        break;
                    }
                    Err(err) => {
                        tracing::warn!(error = ?err, "websocket token revalidation failed; keeping socket open");
                    }
                }
            }

            // A live frame to push out.
            received = rx.recv() => match received {
                Ok(frame) => {
                    let text = match encode_frame(frame) {
                        Ok(text) => text,
                        Err(err) => {
                            // Serializing a JSON value can't realistically fail;
                            // log and skip rather than dropping the connection.
                            tracing::error!(error = %err, "failed to serialize live frame");
                            continue;
                        }
                    };
                    match send_with_timeout(&mut socket, Message::Text(text.into())).await {
                        SendOutcome::Delivered => {}
                        // The client is gone; stop pumping.
                        SendOutcome::Closed => break,
                        // The client hasn't drained the socket in time; treat it like a
                        // dead peer rather than parking here and stalling revalidation.
                        SendOutcome::TimedOut => {
                            tracing::info!("websocket write timed out; closing socket");
                            break;
                        }
                    }
                }
                // The client couldn't keep up and the channel overwrote unread
                // events. Skipping the backlog is the intended degradation for a
                // live tail — note it and keep the connection alive.
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "websocket client lagged; dropped live events");
                }
                // The sender was dropped (engine shutting down): no more events.
                Err(broadcast::error::RecvError::Closed) => break,
            },

            // Client -> server traffic. We accept no commands yet, but must read
            // the socket to observe a close and let axum answer control frames
            // (ping/pong) automatically.
            from_client = socket.recv() => match from_client {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(_)) => break,
                // Ignore any data frames a client sends; there's no protocol for
                // them yet.
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_core::{
        EphemeralFrame, LiveEvent, SenderTrustFrame, SyncStateFrame, UnreadCountsFrame,
        VerificationFrame, VerificationFrameKind,
    };
    use serde_json::Value;

    fn decode(frame: LiveFrame) -> Value {
        serde_json::from_str(&encode_frame(frame).expect("encode")).expect("json")
    }

    #[test]
    fn timeline_frame_keeps_its_wire_shape() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::Timeline(LiveEvent {
            account_id,
            event_id: "$e:localhost".to_owned(),
            room_id: "!r:localhost".to_owned(),
            sender: "@a:localhost".to_owned(),
            state_key: None,
            prev_content: None,
            origin_ts: 42,
            arrival_order: 7,
            event_type: "m.room.message".to_owned(),
            content: None,
            body: Some("hi".to_owned()),
            relates_to: None,
            sender_trust: Some("verified".to_owned()),
        }));
        assert_eq!(v["type"], "timeline.event");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["event_id"], "$e:localhost");
        assert_eq!(v["payload"]["sender_trust"], "verified");
        // A live frame carries arrival order like a timeline read does: a client
        // choosing a read-receipt target must not have to treat a live event as a
        // special case with no position (ADR 0089).
        assert_eq!(v["payload"]["arrival_order"], 7);
    }

    #[test]
    fn sas_frame_carries_emoji_and_decimals() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::Verification(VerificationFrame {
            account_id,
            flow_id: "$flow".to_owned(),
            kind: VerificationFrameKind::Sas,
            target_user_id: "@u:hs".to_owned(),
            target_device_id: Some("DEV".to_owned()),
            emoji: Some(vec![("🐶".to_owned(), "Dog".to_owned())]),
            decimals: Some((1, 2, 3)),
            outcome: None,
        }));
        assert_eq!(v["type"], "verification.sas");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["flow_id"], "$flow");
        assert_eq!(v["payload"]["device_id"], "DEV");
        assert_eq!(v["payload"]["emoji"][0]["symbol"], "🐶");
        assert_eq!(v["payload"]["emoji"][0]["description"], "Dog");
        assert_eq!(v["payload"]["decimals"], serde_json::json!([1, 2, 3]));
        // Fields that don't apply to this stage are omitted, not null.
        assert!(v["payload"].get("reason").is_none());
    }

    #[test]
    fn requested_and_terminal_frames_tag_correctly() {
        let account_id = Uuid::new_v4();
        let requested = decode(LiveFrame::Verification(VerificationFrame {
            account_id,
            flow_id: "$f".to_owned(),
            kind: VerificationFrameKind::Requested,
            target_user_id: "@u:hs".to_owned(),
            target_device_id: Some("DEV".to_owned()),
            emoji: None,
            decimals: None,
            outcome: None,
        }));
        assert_eq!(requested["type"], "verification.requested");
        // No SAS yet → emoji/decimals omitted.
        assert!(requested["payload"].get("emoji").is_none());
        assert!(requested["payload"].get("decimals").is_none());

        let cancelled = decode(LiveFrame::Verification(VerificationFrame {
            account_id,
            flow_id: "$f".to_owned(),
            kind: VerificationFrameKind::Cancelled,
            target_user_id: "@u:hs".to_owned(),
            target_device_id: Some("DEV".to_owned()),
            emoji: None,
            decimals: None,
            outcome: Some("user cancelled".to_owned()),
        }));
        assert_eq!(cancelled["type"], "verification.cancelled");
        assert_eq!(cancelled["payload"]["reason"], "user cancelled");

        let done = decode(LiveFrame::Verification(VerificationFrame {
            account_id,
            flow_id: "$f".to_owned(),
            kind: VerificationFrameKind::Done,
            target_user_id: "@u:hs".to_owned(),
            target_device_id: Some("DEV".to_owned()),
            emoji: None,
            decimals: None,
            outcome: None,
        }));
        assert_eq!(done["type"], "verification.done");
    }

    #[test]
    fn device_state_frame_wire_shape() {
        let account_id = Uuid::new_v4();
        let device_id = Uuid::new_v4();
        let v = decode(LiveFrame::DeviceState(DeviceStateFrame {
            account_id,
            device_id,
            namespace: "drafts".to_owned(),
            entries: vec![
                (
                    "!r:localhost".to_owned(),
                    Some(serde_json::json!({"text": "hi"})),
                ),
                ("!gone:localhost".to_owned(), None),
            ],
            updated_at: chrono::Utc::now(),
        }));
        assert_eq!(v["type"], "device_state.changed");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["device_id"], device_id.to_string());
        assert_eq!(v["payload"]["namespace"], "drafts");
        assert_eq!(v["payload"]["entries"]["!r:localhost"]["text"], "hi");
        // A deletion rides as an explicit null, not an absent key.
        assert!(v["payload"]["entries"]["!gone:localhost"].is_null());
        assert!(v["payload"]["updated_at"].is_string());
    }

    #[test]
    fn ephemeral_frame_wire_shape() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::Ephemeral(EphemeralFrame {
            account_id,
            room_id: Some("!r:localhost".to_owned()),
            event_type: "m.typing".to_owned(),
            content: serde_json::json!({ "user_ids": ["@alice:localhost"] }),
        }));
        assert_eq!(v["type"], "ephemeral.passthrough");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["room_id"], "!r:localhost");
        assert_eq!(v["payload"]["event_type"], "m.typing");
        assert_eq!(v["payload"]["content"]["user_ids"][0], "@alice:localhost");
    }

    #[test]
    fn ephemeral_frame_omits_room_id_when_absent() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::Ephemeral(EphemeralFrame {
            account_id,
            room_id: None,
            event_type: "m.presence".to_owned(),
            content: serde_json::json!({ "presence": "online" }),
        }));
        assert!(v["payload"].get("room_id").is_none());
    }

    #[test]
    fn sender_trust_violation_frame_wire_shape() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::SenderTrustChanged(SenderTrustFrame {
            account_id,
            user_id: "@bob:localhost".to_owned(),
            verification_violation: true,
        }));
        assert_eq!(v["type"], "sender_trust.violation");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["user_id"], "@bob:localhost");
        assert_eq!(v["payload"]["verification_violation"], true);
    }

    #[test]
    fn unread_counts_changed_frame_wire_shape() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::UnreadCountsChanged(UnreadCountsFrame {
            account_id,
            room_id: "!r:localhost".to_owned(),
            notification_count: 3,
            highlight_count: 1,
        }));
        assert_eq!(v["type"], "unread_counts.changed");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["room_id"], "!r:localhost");
        assert_eq!(v["payload"]["notification_count"], 3);
        assert_eq!(v["payload"]["highlight_count"], 1);
    }

    #[test]
    fn sync_state_changed_frame_wire_shape() {
        let account_id = Uuid::new_v4();
        let v = decode(LiveFrame::SyncStateChanged(SyncStateFrame {
            account_id,
            sync_state: "ready",
        }));
        assert_eq!(v["type"], "account.sync_state");
        assert_eq!(v["account_id"], account_id.to_string());
        assert_eq!(v["payload"]["sync_state"], "ready");
    }
}
