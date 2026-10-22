//! Isolated state-only Sliding Sync transport for room image packs.
//!
//! Never submit these responses to SDK sync processing: a second SDK connection
//! can otherwise overwrite room summaries and pagination owned by SyncService.

use std::time::Duration;

use matrix_sdk::ruma::api::client::sync::sync_events::v5::{self, request};
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::{config::RequestConfig, Client};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::engine::{persist_room_state_value, PersistContext};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

// Parent claims and their reciprocal/permission evidence must hydrate on cold
// start as well as update live. Do not request all room members: unverifiable
// non-reciprocal claims are conservatively omitted by discovery.
const REQUIRED_STATE: &[(&str, &str)] = &[
    (axon_core::image_pack::ROOM_PACK_TYPES[0], "*"),
    (axon_core::image_pack::ROOM_PACK_TYPES[1], "*"),
    ("m.space.parent", "*"),
    ("m.space.child", "*"),
    ("m.room.create", ""),
    ("m.room.power_levels", ""),
    ("m.room.member", "$ME"),
];

fn allowed_state(kind: &str, key: &str, local_user: &str) -> bool {
    REQUIRED_STATE.iter().any(|(allowed, state_key)| {
        kind == *allowed
            && (*state_key == "*"
                || key == *state_key
                || (*state_key == "$ME" && key == local_user))
    })
}

fn request(pos: Option<String>, range_end: u32) -> v5::Request {
    let mut request = v5::Request::new();
    request.conn_id = Some("axon-packs".into());
    request.pos = pos;
    request.timeout = Some(Duration::from_secs(30));
    let mut list = request::List::default();
    list.ranges = vec![(0u32.into(), range_end.into())];
    list.room_details.timeline_limit = 0u32.into();
    list.room_details.required_state = REQUIRED_STATE
        .iter()
        .map(|(kind, key)| (StateEventType::from(*kind), (*key).to_owned()))
        .collect();
    let mut filters = request::ListFilters::default();
    filters.is_invite = Some(false);
    list.filters = Some(filters);
    request.lists.insert("packs".into(), list);
    request
}

async fn poll(
    client: &Client,
    request: v5::Request,
) -> Result<v5::Response, matrix_sdk::HttpError> {
    client
        .send(request)
        .with_request_config(
            RequestConfig::new()
                .disable_retry()
                .timeout(REQUEST_TIMEOUT),
        )
        .await
}

/// One worker per account run, canceled and joined before restart.
/// Only allowlisted required_state is projected; all summaries, timelines,
/// account data and extensions in this transport's responses are ignored.
pub(crate) async fn run(client: Client, ctx: PersistContext, cancel: CancellationToken) {
    let account_id = ctx.account_id;
    transport(client.clone(), account_id, cancel, |response| {
        let ctx = ctx.clone();
        let client = client.clone();
        async move {
            for (room_id, room) in response.rooms {
                let mut projection_attempts = 0;
                let mut malformed_events = 0;
                let mut ignored_events = 0;
                for raw in room.required_state {
                    let Ok(event) = raw.deserialize() else {
                        malformed_events += 1;
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.json().get())
                    else {
                        malformed_events += 1;
                        continue;
                    };
                    if !value
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| {
                            allowed_state(
                                kind,
                                value
                                    .get("state_key")
                                    .and_then(serde_json::Value::as_str)
                                    .unwrap_or(""),
                                ctx.local_user_id.as_ref(),
                            )
                        })
                    {
                        ignored_events += 1;
                        continue;
                    }
                    projection_attempts += 1;
                    persist_room_state_value(event, &value, &room_id, &client, &ctx).await;
                }
                if projection_attempts + malformed_events + ignored_events > 0 {
                    tracing::debug!(%account_id, %room_id, projection_attempts, malformed_events,
                        ignored_events, "processed image pack state batch");
                }
            }
        }
    })
    .await;
    tracing::debug!(%account_id, "image pack sync stopped");
}

async fn transport<F, Fut>(
    client: Client,
    account_id: Uuid,
    cancel: CancellationToken,
    mut consume: F,
) where
    F: FnMut(v5::Response) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    tracing::debug!(%account_id, "image pack sync started");
    let mut pos = None;
    let mut range_end = 99u32;
    let mut backoff = Duration::from_secs(2);
    loop {
        let started = std::time::Instant::now();
        let initial = pos.is_none();
        let result = tokio::select! {
            _ = cancel.cancelled() => return,
            result = tokio::time::timeout(REQUEST_TIMEOUT, poll(&client, request(pos.clone(), range_end))) => result,
        };
        let delay = match result {
            Ok(Ok(response)) => {
                let request_elapsed_ms = started.elapsed().as_millis() as u64;
                let room_count = response.rooms.len();
                let state_event_count: usize = response
                    .rooms
                    .values()
                    .map(|room| room.required_state.len())
                    .sum();
                let previous_range_end = range_end;
                let listed_room_count = response
                    .lists
                    .get("packs")
                    .map(|list| u64::from(list.count));
                pos = Some(response.pos.clone());
                if let Some(list) = response.lists.get("packs") {
                    let count = u64::from(list.count).min(u64::from(u32::MAX)) as u32;
                    range_end = range_end.saturating_add(100).min(count.saturating_sub(1));
                }
                let consume_started = std::time::Instant::now();
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = consume(response) => {},
                }
                let recovered = backoff > Duration::from_secs(2);
                if initial || recovered || state_event_count > 0 || range_end != previous_range_end
                {
                    tracing::debug!(%account_id, initial, recovered, room_count, state_event_count,
                        listed_room_count, requested_range_end = previous_range_end, next_range_end = range_end,
                        request_elapsed_ms, processing_elapsed_ms = consume_started.elapsed().as_millis() as u64,
                        "image pack sync progressed");
                }
                backoff = Duration::from_secs(2);
                // A server returning immediately cannot create a busy poll loop.
                Duration::from_secs(1)
            }
            failure => {
                let position_reset = matches!(&failure, Ok(Err(error)) if error.client_api_error_kind() == Some(&matrix_sdk::ruma::api::error::ErrorKind::UnknownPos));
                let http_status = match &failure {
                    Ok(Err(error)) => error
                        .as_client_api_error()
                        .map(|error| error.status_code.as_u16()),
                    _ => None,
                };
                let failure_kind = match &failure {
                    Err(_) => "outer_timeout",
                    Ok(Err(_)) if position_reset => "unknown_position",
                    _ => "request_error",
                };
                if position_reset {
                    pos = None;
                    range_end = 99;
                }
                // Keep the position through transient failures. Restarting a
                // connection on every timeout would repeatedly rehydrate rooms.
                tracing::warn!(%account_id, failure_kind, http_status, position_reset,
                    request_elapsed_ms = started.elapsed().as_millis() as u64,
                    retry_secs = backoff.as_secs(), "image pack sync interrupted; retrying");
                let delay = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                delay
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Request, response::IntoResponse, Json, Router};
    use matrix_sdk::authentication::matrix::MatrixSession;
    use matrix_sdk::store::RoomLoadSettings;
    use matrix_sdk::{SessionMeta, SessionTokens};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    async fn client(address: std::net::SocketAddr) -> Client {
        let client = Client::builder()
            .homeserver_url(format!("http://{address}"))
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .request_config(matrix_sdk::config::RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: "@packs:localhost".parse().unwrap(),
                        device_id: "PACKTEST".into(),
                    },
                    tokens: SessionTokens {
                        access_token: "fixture-token".into(),
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        client
    }

    #[test]
    fn hierarchy_state_allowlist_is_scoped() {
        for (kind, key) in REQUIRED_STATE {
            assert!(allowed_state(
                kind,
                if *key == "$ME" {
                    "@me:example.org"
                } else {
                    key
                },
                "@me:example.org"
            ));
        }
        assert!(allowed_state(
            "m.space.parent",
            "!parent:example.org",
            "@me:example.org"
        ));
        assert!(!allowed_state(
            "m.room.member",
            "@other:example.org",
            "@me:example.org"
        ));
        assert!(!allowed_state(
            "m.room.power_levels",
            "unexpected",
            "@me:example.org"
        ));
        assert!(!allowed_state("m.room.name", "", "@me:example.org"));
    }

    #[tokio::test]
    async fn state_only_sync_requests_both_types_and_delivers_pack_updates() {
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let app = Router::new().fallback({
            let requests = requests.clone();
            move |request: Request| {
                let requests = requests.clone();
                let count = count.clone();
                async move {
                    if !request.uri().path().ends_with("/sync") {
                        return (axum::http::StatusCode::NOT_FOUND, Json(json!({}))).into_response();
                    }
                    let body = axum::body::to_bytes(request.into_body(), 8192).await.unwrap();
                    requests.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    let kind = if n == 0 { "im.ponies.room_emotes" } else { "m.room.image_pack" };
                    Json(json!({"pos": (n+1).to_string(), "lists": {"packs": {"count": 1, "ops": [{"op":"SYNC", "range":[0,0], "room_ids":["!pack:localhost"]}]}},
                        "rooms": {"!pack:localhost": {"initial": n == 0, "name": "must not overwrite", "prev_batch": "foreign-page", "required_state": [{
                            "type": kind, "state_key": "custom", "event_id": format!("$pack{n}"),
                            "sender": "@packs:localhost", "origin_server_ts": n+1,
                            "content": {"images": if n == 0 { json!({"allthethings": {"url": "mxc://example.org/x"}}) } else { json!({}) }}
                        }]}}
                    })).into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = client(address).await;
        let mut events = Vec::<Value>::new();
        let mut pos = None;
        for _ in 0..2 {
            let response =
                tokio::time::timeout(Duration::from_secs(5), poll(&client, request(pos, 99)))
                    .await
                    .unwrap()
                    .unwrap();
            pos = Some(response.pos.clone());
            for room in response.rooms.into_values() {
                events.extend(
                    room.required_state
                        .into_iter()
                        .map(|raw| serde_json::from_str(raw.json().get()).unwrap()),
                );
            }
        }
        // Even a response with room metadata must not enter SDK room processing.
        assert!(client
            .get_room(matrix_sdk::ruma::room_id!("!pack:localhost"))
            .is_none());
        let requests = requests.lock().unwrap();
        let list = &requests[0]["lists"]["packs"];
        assert_eq!(requests[0]["conn_id"], "axon-packs");
        assert_eq!(list["timeline_limit"], 0);
        assert_eq!(list["filters"]["is_invite"], false);
        assert_eq!(
            list["required_state"],
            json!([
                ["m.room.image_pack", "*"],
                ["im.ponies.room_emotes", "*"],
                ["m.space.parent", "*"],
                ["m.space.child", "*"],
                ["m.room.create", ""],
                ["m.room.power_levels", ""],
                ["m.room.member", "$ME"]
            ])
        );
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["content"]["images"]["allthethings"]["url"],
            "mxc://example.org/x"
        );
        assert_eq!(events[1]["content"]["images"], json!({}));
        server.abort();
    }

    #[tokio::test]
    async fn pack_poll_preserves_primary_room_metadata_and_pagination() {
        let app = Router::new().fallback(|request: Request| async move {
            let body = axum::body::to_bytes(request.into_body(), 8192)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            let primary = body["conn_id"] == "primary";
            let name = if primary { "Primary" } else { "Foreign" };
            let list = if primary { "main" } else { "packs" };
            Json(json!({"pos":"1", "lists": {list: {"count":1}},
                "rooms":{"!pack:localhost":{
                    "initial":true,"name":name,"prev_batch":name,
                    "bump_stamp": if primary { 100 } else { 900 },
                    "required_state":[{"type":"m.room.name","state_key":"",
                        "sender":"@packs:localhost","event_id":format!("${name}"),
                        "origin_server_ts":1,"content":{"name":name}}]
                }}
            }))
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = client(address).await;
        let primary = client
            .sliding_sync("primary")
            .unwrap()
            .add_list(
                matrix_sdk::SlidingSyncList::builder("main")
                    .sync_mode(matrix_sdk::SlidingSyncMode::new_growing(100)),
            )
            .build()
            .await
            .unwrap();
        primary.sync_once().await.unwrap();
        let room = client
            .get_room(matrix_sdk::ruma::room_id!("!pack:localhost"))
            .unwrap();
        assert_eq!(room.name().as_deref(), Some("Primary"));
        assert_eq!(room.last_prev_batch().as_deref(), Some("Primary"));
        let recency = room.recency_stamp();
        poll(&client, request(None, 99)).await.unwrap();
        assert_eq!(room.name().as_deref(), Some("Primary"));
        assert_eq!(room.last_prev_batch().as_deref(), Some("Primary"));
        assert_eq!(room.recency_stamp(), recency);
        server.abort();
    }

    #[tokio::test]
    async fn worker_cancels_a_hung_request() {
        let seen = Arc::new(tokio::sync::Notify::new());
        let app = Router::new().fallback({
            let seen = seen.clone();
            move || {
                let seen = seen.clone();
                async move {
                    seen.notify_one();
                    std::future::pending::<()>().await;
                    Json(json!({}))
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(transport(
            client(address).await,
            Uuid::new_v4(),
            cancel.clone(),
            |_| async {},
        ));
        tokio::time::timeout(Duration::from_secs(5), seen.notified())
            .await
            .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }
}
