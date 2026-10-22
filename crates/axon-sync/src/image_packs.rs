//! State-only Sliding Sync acquisition for room image packs.

use std::time::Duration;

use futures_util::{pin_mut, StreamExt};
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::{Client, SlidingSync, SlidingSyncList, SlidingSyncMode};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn connection(client: &Client) -> matrix_sdk::Result<SlidingSync> {
    let mut filters =
        matrix_sdk::ruma::api::client::sync::sync_events::v5::request::ListFilters::default();
    filters.is_invite = Some(false);
    client
        .sliding_sync("axon-packs")?
        .add_list(
            SlidingSyncList::builder("packs")
                .sync_mode(SlidingSyncMode::new_growing(100))
                .timeline_limit(0)
                .filters(Some(filters))
                .required_state(
                    axon_core::image_pack::ROOM_PACK_TYPES
                        .into_iter()
                        .map(|kind| (StateEventType::from(kind), "*".to_owned()))
                        .collect(),
                ),
        )
        .build()
        .await
}

/// Exactly one connection per active account run, owned/canceled/joined by
/// run_account. Ordinary raw-state handlers persist its updates, including
/// redaction and leave handling. Nothing is inferred from missing state.
pub(crate) async fn run(client: Client, account_id: Uuid, cancel: CancellationToken) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let built = tokio::select! {
            _ = cancel.cancelled() => return,
            result = connection(&client) => result,
        };
        if let Ok(sync) = built {
            let stream = sync.sync();
            pin_mut!(stream);
            loop {
                // Covers SDK retries as well as the long poll; a slow peer
                // cannot permanently consume the account's single worker.
                let next = tokio::select! {
                    _ = cancel.cancelled() => return,
                    result = tokio::time::timeout(Duration::from_secs(90), stream.next()) => result,
                };
                match next {
                    Ok(Some(Ok(update))) => {
                        backoff = Duration::from_secs(2);
                        if !update.rooms.is_empty() {
                            tracing::debug!(%account_id, updated_rooms = update.rooms.len(), "image pack sync state updated");
                        }
                    }
                    _ => break,
                }
            }
        }
        // A failed pack connection does not stop message sync. Avoid logging
        // untrusted remote error strings or account data content.
        tracing::warn!(%account_id, retry_secs = backoff.as_secs(), "image pack sync interrupted; retrying");
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Request, response::IntoResponse, Json, Router};
    use matrix_sdk::authentication::matrix::MatrixSession;
    use matrix_sdk::event_handler::RawEvent;
    use matrix_sdk::ruma::events::AnySyncStateEvent;
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
                        "rooms": {"!pack:localhost": {"initial": n == 0, "required_state": [{
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
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        client.add_event_handler({
            let events = events.clone();
            move |_: AnySyncStateEvent, raw: RawEvent| {
                let events = events.clone();
                async move {
                    events
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(raw.get()).unwrap());
                }
            }
        });
        let sync = connection(&client).await.unwrap();
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), sync.sync_once())
                .await
                .unwrap()
                .unwrap();
        }
        let requests = requests.lock().unwrap();
        let list = &requests[0]["lists"]["packs"];
        assert_eq!(requests[0]["conn_id"], "axon-packs");
        assert_eq!(list["timeline_limit"], 0);
        assert_eq!(list["filters"]["is_invite"], false);
        assert_eq!(
            list["required_state"],
            json!([["m.room.image_pack", "*"], ["im.ponies.room_emotes", "*"]])
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["content"]["images"]["allthethings"]["url"],
            "mxc://example.org/x"
        );
        assert_eq!(events[1]["content"]["images"], json!({}));
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
        let task = tokio::spawn(run(client(address).await, Uuid::new_v4(), cancel.clone()));
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
