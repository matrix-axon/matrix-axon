//! Matrix-link cards (ADR 0138): `GET …/rooms/{room_id}/events/{event_id}`,
//! and the media routes' `room_id`/`event_id` scope. The homeserver read itself
//! (decryption, redactions, bundled edits, error mapping) is pinned in
//! `axon-sync`'s `linked_event` module; here it is a stub.
use super::*;

use axon_api::{FetchedEvent, LinkedEventError, LinkedEventSource};

/// A linked-event source that answers from a fixed result and records reads.
struct StubSource {
    answer: Box<dyn Fn() -> Result<FetchedEvent, LinkedEventError> + Send + Sync>,
    reads: Mutex<Vec<(Uuid, String, String)>>,
}

impl StubSource {
    fn new(
        answer: impl Fn() -> Result<FetchedEvent, LinkedEventError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            answer: Box::new(answer),
            reads: Mutex::new(Vec::new()),
        })
    }

    fn reads(&self) -> Vec<(Uuid, String, String)> {
        self.reads.lock().unwrap().clone()
    }
}

#[async_trait]
impl LinkedEventSource for StubSource {
    async fn read(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<FetchedEvent, LinkedEventError> {
        self.reads
            .lock()
            .unwrap()
            .push((account_id, room_id.to_owned(), event_id.to_owned()));
        (self.answer)()
    }
}

fn fetched(room_id: &str, event_id: &str, content: Value) -> FetchedEvent {
    FetchedEvent {
        event_id: event_id.to_owned(),
        room_id: room_id.to_owned(),
        sender: "@bob:localhost".to_owned(),
        origin_ts: 1_700_000_000_000,
        event_type: "m.room.message".to_owned(),
        body: content["body"].as_str().map(str::to_owned),
        content: Some(content),
        relates_to: None,
        redacted: false,
        edited: true,
        sender_trust: Some("mismatched_sender".to_owned()),
    }
}

fn app(
    store: Store,
    source: Arc<dyn LinkedEventSource>,
    media: Arc<dyn MediaProxy>,
) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(
        AppState::new(
            store,
            live,
            Arc::new(StubSender::ok("$unused:localhost")),
            Arc::new(StubLifecycle::ok(Uuid::nil())),
            Arc::new(StubVerification::ok("$unused-flow")),
            Arc::new(StubTrust::ok()),
            Arc::new(StubDeviceList::ok()),
            Arc::new(StubTokenVerifier::ok()),
            media,
            None,
        )
        .with_linked_events(source),
    )
}

async fn account(store: &Store) -> Uuid {
    store
        .upsert_account(
            &format!("@cards-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap()
        .account_id
}

fn uri(account_id: Uuid, room_id: &str, event_id: &str) -> String {
    format!("/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}")
}

fn never() -> Arc<StubSource> {
    StubSource::new(|| Err(LinkedEventError::Internal))
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_stored_event_comes_from_the_store_as_the_timeline_shows_it() {
    let store = store().await;
    let account_id = account(&store).await;
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account_id, &room, 1_000, "first").await;
    insert_relation(
        &store,
        account_id,
        &room,
        "@alice:localhost",
        2_000,
        "m.room.message",
        json!({
            "msgtype": "m.text",
            "body": "* second",
            "m.new_content": {"msgtype": "m.text", "body": "second"},
        }),
        json!({"rel_type": "m.replace", "event_id": event_id}),
    )
    .await;
    let source = never();
    let app = app(store, source.clone(), Arc::new(StubMediaProxy));

    let (status, body) = get(&app, &uri(account_id, &room, &event_id)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = &body["data"];
    assert_eq!(data["event_id"], event_id.as_str());
    assert_eq!(data["room_id"], room.as_str());
    assert_eq!(data["body"], "second");
    assert_eq!(data["edited"], true);
    assert_eq!(data["redacted"], false);
    assert_eq!(data["source"], "store");
    assert!(data.get("arrival_order").is_none(), "{data}");
    assert!(source.reads().is_empty(), "the homeserver is not asked");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_stored_redacted_event_is_masked() {
    let store = store().await;
    let account_id = account(&store).await;
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account_id, &room, 1_000, "secret").await;
    insert_redaction(&store, account_id, &room, 2_000, &event_id).await;
    let app = app(store, never(), Arc::new(StubMediaProxy));

    let (status, body) = get(&app, &uri(account_id, &room, &event_id)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["redacted"], true);
    assert_eq!(body["data"]["content"], Value::Null);
    assert_eq!(body["data"]["body"], Value::Null);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_stored_event_in_another_room_is_not_found_without_asking() {
    let store = store().await;
    let account_id = account(&store).await;
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let event_id = insert_message(&store, account_id, &room, 1_000, "hello").await;
    let source = never();
    let app = app(store, source.clone(), Arc::new(StubMediaProxy));

    let (status, body) = get(&app, &uri(account_id, "!elsewhere:localhost", &event_id)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(source.reads().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_unstored_event_is_read_from_the_homeserver() {
    let store = store().await;
    let account_id = account(&store).await;
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let answer_room = room.clone();
    let source = StubSource::new(move || {
        Ok(fetched(
            &answer_room,
            "$remote:localhost",
            json!({"msgtype": "m.text", "body": "from the homeserver"}),
        ))
    });
    let app = app(store, source.clone(), Arc::new(StubMediaProxy));

    let (status, body) = get(&app, &uri(account_id, &room, "$remote:localhost")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data = &body["data"];
    assert_eq!(data["source"], "homeserver");
    assert_eq!(data["body"], "from the homeserver");
    assert_eq!(data["edited"], true);
    assert_eq!(data["sender_trust"], "mismatched_sender");
    assert_eq!(data["type"], "m.room.message");
    assert_eq!(
        source.reads(),
        vec![(account_id, room.clone(), "$remote:localhost".to_owned())]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn homeserver_failures_keep_their_statuses() {
    let store = store().await;
    let account_id = account(&store).await;
    for (error, status) in [
        (
            (|| LinkedEventError::NotFound("gone".into())) as fn() -> LinkedEventError,
            StatusCode::NOT_FOUND,
        ),
        (
            || LinkedEventError::Invalid("bad id".into()),
            StatusCode::BAD_REQUEST,
        ),
        (
            || LinkedEventError::NotActive("logged out".into()),
            StatusCode::CONFLICT,
        ),
        (
            || LinkedEventError::Unavailable("busy".into()),
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            || LinkedEventError::Upstream("502".into()),
            StatusCode::BAD_GATEWAY,
        ),
        (
            || LinkedEventError::Timeout("slow".into()),
            StatusCode::GATEWAY_TIMEOUT,
        ),
    ] {
        let source = StubSource::new(move || Err(error()));
        let app = app(store.clone(), source, Arc::new(StubMediaProxy));
        let (got, body) = get(&app, &uri(account_id, "!r:localhost", "$e:localhost")).await;
        assert_eq!(got, status, "{body}");
    }
}

fn image(mxc: &str, mimetype: &str) -> Value {
    json!({
        "msgtype": "m.image",
        "body": "cat.png",
        "url": mxc,
        "info": {"mimetype": mimetype},
    })
}

/// Section 4: a card's image is served through the event that names it, with
/// that event's MIME type, even when another stored event names the same URI
/// with a different one.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_fetched_events_media_is_served_through_that_event_alone() {
    let store = store().await;
    let account_id = account(&store).await;
    let media_id = Uuid::new_v4().simple().to_string();
    let mxc = format!("mxc://example.org/{media_id}");
    // Another event, in a room the account can read, names the same URI with
    // a type that would be served differently.
    insert_media_event(
        &store,
        account_id,
        "!other:localhost",
        &mxc,
        Some("video/mp4"),
    )
    .await;
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let answer_room = room.clone();
    let answer_mxc = mxc.clone();
    let source = StubSource::new(move || {
        Ok(fetched(
            &answer_room,
            "$img:localhost",
            image(&answer_mxc, "image/png"),
        ))
    });
    let media = Arc::new(ConfiguredMediaProxy::ok(b"png bytes"));
    let app = app(store, source, media.clone());
    let media_uri = |query: &str| format!("/v1/media/{account_id}/example.org/{media_id}{query}");
    let scope = format!("?room_id={room}&event_id=%24img:localhost");

    // Before the card read, nothing is kept for the event.
    let (status, _, _) = get_media_bytes(&app, &media_uri(&scope), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = get(&app, &uri(account_id, &room, "$img:localhost")).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, headers, bytes) = get_media_bytes(&app, &media_uri(&scope), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"png bytes");
    assert_eq!(headers["content-type"], "image/png");

    // Unscoped, the route resolves the URI as before, through the stored event.
    let (status, headers, _) = get_media_bytes(&app, &media_uri(""), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "video/mp4");

    // The scope binds: another URI is not served through this event.
    let other = format!("/v1/media/{account_id}/example.org/other{scope}");
    let (status, _, _) = get_media_bytes(&app, &other, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Nor is this event's media served to another account.
    let stranger = Uuid::new_v4();
    let (status, _, _) = get_media_bytes(
        &app,
        &format!("/v1/media/{stranger}/example.org/{media_id}{scope}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_stored_events_media_is_served_through_that_event_alone() {
    let store = store().await;
    let account_id = account(&store).await;
    let media_id = Uuid::new_v4().simple().to_string();
    let mxc = format!("mxc://example.org/{media_id}");
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let named = insert_media_event(&store, account_id, &room, &mxc, Some("image/png")).await;
    let unrelated = insert_message(&store, account_id, &room, 1, "no media").await;
    let media = Arc::new(ConfiguredMediaProxy::ok(b"png bytes"));
    let app = app(store, never(), media);
    let media_uri = |room: &str, event: &str| {
        format!(
            "/v1/media/{account_id}/example.org/{media_id}?room_id={room}&event_id={}",
            event.replace('$', "%24")
        )
    };

    let (status, headers, _) = get_media_bytes(&app, &media_uri(&room, &named), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    let (status, _, _) = get_media_bytes(&app, &media_uri(&room, &unrelated), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the event must name the URI");
    let (status, _, _) =
        get_media_bytes(&app, &media_uri("!elsewhere:localhost", &named), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the event must be in the room"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn one_scope_parameter_alone_is_a_bad_request() {
    let store = store().await;
    let account_id = account(&store).await;
    let media_id = Uuid::new_v4().simple().to_string();
    let mxc = format!("mxc://example.org/{media_id}");
    insert_media_event(&store, account_id, "!r:localhost", &mxc, Some("image/png")).await;
    let app = app(store, never(), Arc::new(ConfiguredMediaProxy::ok(b"png")));
    for query in ["?room_id=!r:localhost", "?event_id=%24e:localhost"] {
        for suffix in ["", "/thumbnail"] {
            let thumb = if suffix.is_empty() {
                ""
            } else {
                "&width=32&height=32"
            };
            let path =
                format!("/v1/media/{account_id}/example.org/{media_id}{suffix}{query}{thumb}");
            let (status, _, _) = get_media_bytes(&app, &path, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        }
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_thumbnail_is_served_through_a_fetched_event() {
    let store = store().await;
    let account_id = account(&store).await;
    let media_id = Uuid::new_v4().simple().to_string();
    let room = format!("!cards-{}:localhost", Uuid::new_v4());
    let answer_room = room.clone();
    let thumb = format!("mxc://example.org/{media_id}");
    let source = StubSource::new(move || {
        Ok(fetched(
            &answer_room,
            "$vid:localhost",
            json!({
                "msgtype": "m.video",
                "body": "clip.mp4",
                "url": "mxc://example.org/video",
                "info": {
                    "mimetype": "video/mp4",
                    "thumbnail_url": thumb,
                    "thumbnail_info": {"mimetype": "image/jpeg"},
                },
            }),
        ))
    });
    let media = Arc::new(ConfiguredMediaProxy::ok(b"\xff\xd8\xff poster"));
    let app = app(store, source, media.clone());
    let (status, _) = get(&app, &uri(account_id, &room, "$vid:localhost")).await;
    assert_eq!(status, StatusCode::OK);

    let path = format!(
        "/v1/media/{account_id}/example.org/{media_id}/thumbnail?width=320&height=240&room_id={room}&event_id=%24vid:localhost"
    );
    let (status, _, _) = get_media_bytes(&app, &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(media.thumbnail_calls().len(), 1);
}
