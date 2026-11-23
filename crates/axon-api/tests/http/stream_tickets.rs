//! Media stream tickets (ADR 0139 section 5): minting behind the bearer gate,
//! serving outside it, the per-use re-checks, and keeping the ticket out of
//! the log.

use super::*;

const DATA: &[u8] = b"0123456789abcdef";

/// [`media_app`], with the token verifier the test holds the revocation
/// handle of.
fn ticket_app(
    store: Store,
    media: Arc<dyn MediaProxy>,
    verifier: StubTokenVerifier,
) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    axon_api::router(AppState::new(
        store,
        live,
        Arc::new(StubSender::ok("$unused:localhost")),
        Arc::new(StubLifecycle::ok(Uuid::nil())),
        Arc::new(StubVerification::ok("$unused-flow")),
        Arc::new(StubTrust::ok()),
        Arc::new(StubDeviceList::ok()),
        Arc::new(verifier),
        media,
        None,
    ))
}

struct Fixture {
    store: Store,
    account_id: Uuid,
    room_id: String,
}

impl Fixture {
    async fn new() -> Self {
        let store = store().await;
        let account_id = store
            .upsert_account(
                &format!("@ticket-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .expect("account")
            .account_id;
        let room_id = format!("!ticket-{}:localhost", Uuid::new_v4());
        Self {
            store,
            account_id,
            room_id,
        }
    }

    /// A stored event owning a fresh MXC URI of type `mimetype`: the event id
    /// and the URI.
    async fn media(&self, mimetype: &str) -> (String, String) {
        let mxc = format!("mxc://example.org/{}", Uuid::new_v4().simple());
        let event_id = insert_media_event(
            &self.store,
            self.account_id,
            &self.room_id,
            &mxc,
            Some(mimetype),
        )
        .await;
        (event_id, mxc)
    }

    fn mint_uri(&self, mxc: &str) -> String {
        format!(
            "/v1/media/{}/{}/stream-ticket",
            self.account_id,
            mxc.trim_start_matches("mxc://")
        )
    }
}

/// Mint a ticket with the bearer token: the status and body.
async fn mint(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = send_request(app, "POST", uri, None, Some(&bearer()), None).await;
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, body)
}

/// Mint a ticket that must succeed: its path.
async fn mint_path(app: &axum::Router, uri: &str) -> String {
    let (status, body) = mint(app, uri).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]["path"].as_str().expect("path").to_owned()
}

/// Load a ticket path as a `<video>` would: no `Authorization` header.
async fn play(
    app: &axum::Router,
    path: &str,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    send_request(app, "GET", path, None, None, range.map(|r| ("range", r))).await
}

fn assert_ticket_headers(headers: &HeaderMap) {
    assert_eq!(headers["cache-control"], "private, no-store", "{headers:?}");
    assert_eq!(headers["referrer-policy"], "no-referrer", "{headers:?}");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_ticket_serves_its_video_without_a_bearer_token() {
    let fx = Fixture::new().await;
    let (_, mxc) = fx.media("video/mp4").await;
    let proxy = Arc::new(ConfiguredMediaProxy::ok(DATA));
    let app = ticket_app(fx.store.clone(), proxy.clone(), StubTokenVerifier::ok());

    let before = chrono::Utc::now();
    let (status, body) = mint(&app, &fx.mint_uri(&mxc)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let path = body["data"]["path"].as_str().expect("path");
    let ticket = path
        .strip_prefix("/v1/media-stream/")
        .expect("a relative ticket path");
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        ticket.as_bytes(),
    )
    .expect("base64url");
    assert_eq!(decoded.len(), 32);
    // A non-expiring token: the ticket's own idle expiry.
    let expires_at = chrono::DateTime::parse_from_rfc3339(
        body["data"]["expires_at"].as_str().expect("expires_at"),
    )
    .expect("RFC 3339");
    let idle = expires_at.signed_duration_since(before);
    assert!(
        idle > chrono::Duration::minutes(29) && idle <= chrono::Duration::minutes(31),
        "{idle:?}"
    );

    let (status, headers, bytes) = play(&app, path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, DATA);
    assert_eq!(headers["content-type"], "video/mp4");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["accept-ranges"], "bytes");
    assert_ticket_headers(&headers);
    assert_eq!(proxy.stream_calls().len(), 1, "served by the streamed path");
    assert_eq!(proxy.stream_calls()[0].account_id, fx.account_id);
    assert_eq!(proxy.stream_calls()[0].mxc_url, mxc);

    let (status, headers, bytes) = play(&app, path, Some("bytes=2-5")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 2-5/16");
    assert_eq!(bytes, b"2345");
    assert_ticket_headers(&headers);

    // A bearer header changes nothing: the ticket is the credential.
    let (status, _, bytes) =
        send_request(&app, "GET", path, None, Some("Bearer wrong"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, DATA);

    fx.store.delete_account_row(fx.account_id).await.unwrap();
}

/// The ticket route answers ranges on a download still in flight exactly as
/// the bearer route does, because it serves through the same code.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_ticket_serves_ranges_of_a_download_in_flight() {
    let fx = Fixture::new().await;
    let (_, mxc) = fx.media("audio/ogg").await;
    let growing = |declared, learned| GrowingOutcome {
        data: DATA.to_vec(),
        declared,
        learned,
        fails: false,
    };
    for (outcome, range, status, content_range, body) in [
        (
            growing(true, false),
            Some("bytes=2-5"),
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 2-5/16"),
            &b"2345"[..],
        ),
        (
            growing(false, true),
            Some("bytes=-4"),
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 12-15/16"),
            &b"cdef"[..],
        ),
        (growing(false, false), None, StatusCode::OK, None, DATA),
        (
            growing(true, false),
            Some("bytes=100-"),
            StatusCode::RANGE_NOT_SATISFIABLE,
            Some("bytes */16"),
            &b""[..],
        ),
    ] {
        let app = ticket_app(
            fx.store.clone(),
            Arc::new(ConfiguredMediaProxy::growing(outcome)),
            StubTokenVerifier::ok(),
        );
        let path = mint_path(&app, &fx.mint_uri(&mxc)).await;
        let (got, headers, bytes) = play(&app, &path, range).await;
        assert_eq!(got, status, "{range:?}");
        assert_eq!(
            headers.get("content-range").map(|v| v.to_str().unwrap()),
            content_range,
            "{range:?}"
        );
        if status != StatusCode::RANGE_NOT_SATISFIABLE {
            assert_eq!(bytes, body, "{range:?}");
        }
        assert_ticket_headers(&headers);
    }
    fx.store.delete_account_row(fx.account_id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn only_video_and_audio_with_an_owning_event_get_a_ticket() {
    let fx = Fixture::new().await;
    let proxy = Arc::new(ConfiguredMediaProxy::ok(DATA));
    let app = ticket_app(fx.store.clone(), proxy.clone(), StubTokenVerifier::ok());

    for mimetype in ["video/webm", "audio/mpeg"] {
        let (_, mxc) = fx.media(mimetype).await;
        let (status, body) = mint(&app, &fx.mint_uri(&mxc)).await;
        assert_eq!(status, StatusCode::OK, "{mimetype}: {body}");
    }
    // Served as something else: an image, or a type the media route would
    // downgrade to `application/octet-stream`.
    for mimetype in ["image/png", "text/html", "application/pdf"] {
        let (_, mxc) = fx.media(mimetype).await;
        let (status, body) = mint(&app, &fx.mint_uri(&mxc)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{mimetype}: {body}");
        assert_eq!(body["error"]["code"], "bad_request");
    }
    // A custom reaction's key is served as a sniffed raster image only.
    let reaction = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    insert_custom_reaction(&fx.store, fx.account_id, &fx.room_id, &reaction).await;
    let (status, _) = mint(&app, &fx.mint_uri(&reaction)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // No stored event owns it.
    let unowned = format!("mxc://example.org/{}", Uuid::new_v4().simple());
    let (status, _) = mint(&app, &fx.mint_uri(&unowned)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Another account's event does not count.
    let (_, mxc) = fx.media("video/mp4").await;
    let other = format!(
        "/v1/media/{}/{}/stream-ticket",
        Uuid::new_v4(),
        mxc.trim_start_matches("mxc://")
    );
    let (status, _) = mint(&app, &other).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Minting needs the bearer token.
    let (status, _, _) = send_request(&app, "POST", &fx.mint_uri(&mxc), None, None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    assert!(proxy.calls().is_empty() && proxy.stream_calls().is_empty());
    fx.store.delete_account_row(fx.account_id).await.unwrap();
}

/// A parameterized or mixed-case video or audio type gets a ticket, and the
/// ticket serves it as its lowercased essence (ADR 0142 section 1: Axon sends
/// the declared type, parameters included).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_parameterized_video_or_audio_type_gets_a_ticket_and_serves_its_essence() {
    let fx = Fixture::new().await;
    let proxy = Arc::new(ConfiguredMediaProxy::ok(DATA));
    let app = ticket_app(fx.store.clone(), proxy.clone(), StubTokenVerifier::ok());

    for (declared, served) in [
        ("audio/ogg; codecs=opus", "audio/ogg"),
        ("video/webm; codecs=vp9", "video/webm"),
        ("Video/MP4", "video/mp4"),
    ] {
        let (_, mxc) = fx.media(declared).await;
        let path = mint_path(&app, &fx.mint_uri(&mxc)).await;
        let (status, headers, bytes) = play(&app, &path, None).await;
        assert_eq!(status, StatusCode::OK, "{declared}");
        assert_eq!(bytes, DATA, "{declared}");
        assert_eq!(headers["content-type"], served, "{declared}");
    }
    // A parameter does not make an unsafe type ticketable.
    let (_, mxc) = fx.media("text/html; charset=utf-8").await;
    let (status, _) = mint(&app, &fx.mint_uri(&mxc)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    fx.store.delete_account_row(fx.account_id).await.unwrap();
}

/// A ticket is accepted exactly when the token that issued it would be, and
/// a refused ticket never comes back.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_ticket_stops_working_once_its_token_is_revoked() {
    let fx = Fixture::new().await;
    let (_, mxc) = fx.media("video/mp4").await;
    let verifier = StubTokenVerifier::ok();
    let active = verifier.revocation_handle();
    let proxy = Arc::new(ConfiguredMediaProxy::ok(DATA));
    let app = ticket_app(fx.store.clone(), proxy.clone(), verifier);
    let path = mint_path(&app, &fx.mint_uri(&mxc)).await;
    assert_eq!(play(&app, &path, None).await.0, StatusCode::OK);

    active.store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, headers, body) = play(&app, &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ticket_headers(&headers);
    let body: Value = serde_json::from_slice(&body).expect("error envelope");
    assert_eq!(body["error"]["code"], "not_found");

    active.store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        play(&app, &path, None).await.0,
        StatusCode::NOT_FOUND,
        "a refused ticket is forgotten"
    );
    assert_eq!(
        proxy.stream_calls().len(),
        1,
        "nothing served after revocation"
    );
    fx.store.delete_account_row(fx.account_id).await.unwrap();
}

/// Every use re-runs the media route's own checks, so a redaction or a
/// removed account ends the ticket at its next request.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_ticket_stops_working_once_its_event_is_redacted_or_account_removed() {
    let fx = Fixture::new().await;
    let (event_id, mxc) = fx.media("video/mp4").await;
    let (_, kept) = fx.media("audio/ogg").await;
    let proxy = Arc::new(ConfiguredMediaProxy::ok(DATA));
    let app = ticket_app(fx.store.clone(), proxy.clone(), StubTokenVerifier::ok());
    let redacted = mint_path(&app, &fx.mint_uri(&mxc)).await;
    let removed = mint_path(&app, &fx.mint_uri(&kept)).await;
    assert_eq!(play(&app, &redacted, None).await.0, StatusCode::OK);
    assert_eq!(play(&app, &removed, None).await.0, StatusCode::OK);

    insert_redaction(
        &fx.store,
        fx.account_id,
        &fx.room_id,
        1_700_000_000_001,
        &event_id,
    )
    .await;
    let (status, headers, _) = play(&app, &redacted, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ticket_headers(&headers);
    assert_eq!(play(&app, &removed, None).await.0, StatusCode::OK);

    fx.store.delete_account_row(fx.account_id).await.unwrap();
    assert_eq!(play(&app, &removed, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(proxy.stream_calls().len(), 3);
}

/// Only a request served without error counts as a use. A ticket whose
/// requests keep failing stays the least recently used, so its session's next
/// ticket evicts it rather than one that plays.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn only_a_served_request_counts_as_a_use_of_its_ticket() {
    let fx = Fixture::new().await;
    let (event_id, failing) = fx.media("video/mp4").await;
    let (_, playing) = fx.media("video/mp4").await;
    let app = ticket_app(
        fx.store.clone(),
        Arc::new(ConfiguredMediaProxy::ok(DATA)),
        StubTokenVerifier::ok(),
    );
    // One bearer token, so one session: its oldest ticket is for media whose
    // event is then redacted, and the rest fill the session's cap.
    let oldest = mint_path(&app, &fx.mint_uri(&failing)).await;
    let rest: Vec<String> = {
        let mut rest = Vec::new();
        for _ in 1..32 {
            rest.push(mint_path(&app, &fx.mint_uri(&playing)).await);
        }
        rest
    };
    insert_redaction(
        &fx.store,
        fx.account_id,
        &fx.room_id,
        1_700_000_000_001,
        &event_id,
    )
    .await;
    for _ in 0..3 {
        assert_eq!(play(&app, &oldest, None).await.0, StatusCode::NOT_FOUND);
    }
    // A served request is a use: the second-oldest is no longer next out.
    assert_eq!(play(&app, &rest[0], None).await.0, StatusCode::OK);

    mint_path(&app, &fx.mint_uri(&playing)).await;
    assert_eq!(
        play(&app, &rest[0], None).await.0,
        StatusCode::OK,
        "a played ticket is kept"
    );
    assert_eq!(
        play(&app, &rest[1], None).await.0,
        StatusCode::OK,
        "the failing ticket went instead of the next-oldest"
    );

    // The next eviction takes the oldest ticket not played since.
    mint_path(&app, &fx.mint_uri(&playing)).await;
    assert_eq!(play(&app, &rest[2], None).await.0, StatusCode::NOT_FOUND);
    fx.store.delete_account_row(fx.account_id).await.unwrap();
}

#[derive(Clone)]
struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Run `requests` under a subscriber recording everything down to `TRACE`:
/// the log text.
async fn logged<F: std::future::Future<Output = ()>>(requests: F) -> String {
    use tracing::instrument::WithSubscriber;

    let output = Arc::new(Mutex::new(Vec::new()));
    let writer = LogWriter(output.clone());
    let subscriber = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish(),
    );
    // While exactly one dispatcher is registered, tracing-core registers a
    // callsite against the default subscriber of whichever thread hits it
    // first. A parallel test's thread has none, so an event this test expects
    // would be cached as "never" and dropped. A second, inert dispatcher kept
    // alive makes every callsite's interest account for this subscriber.
    let inert = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    requests.with_subscriber(subscriber).await;
    drop(inert);
    let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    logs
}

/// Tickets are never logged (ADR 0139 section 5): not on a served request,
/// not on a refused one, and not under the prefix where no route matches and
/// the `/v1/{*path}` catch-all answers. Nor does an error envelope repeat it.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_ticket_never_reaches_the_log_or_an_error_body() {
    let fx = Fixture::new().await;
    let (_, mxc) = fx.media("video/mp4").await;
    let verifier = StubTokenVerifier::ok();
    let active = verifier.revocation_handle();
    let app = ticket_app(
        fx.store.clone(),
        Arc::new(ConfiguredMediaProxy::ok(DATA)),
        verifier,
    );
    let path = mint_path(&app, &fx.mint_uri(&mxc)).await;
    let ticket = path.strip_prefix("/v1/media-stream/").unwrap().to_owned();

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let logs = logged({
        let (app, path, bodies) = (app.clone(), path.clone(), bodies.clone());
        async move {
            let (status, _, body) = play(&app, &path, Some("bytes=0-3")).await;
            assert_eq!(status, StatusCode::PARTIAL_CONTENT);
            assert_eq!(body, b"0123");
            // A deeper path is this route's 404, not the bearer-gated `/v1`
            // catch-all's 401, so it carries the ticket headers too.
            for (method, uri, auth, status) in [
                ("GET", format!("{path}/extra"), None, StatusCode::NOT_FOUND),
                (
                    "GET",
                    format!("{path}/extra"),
                    Some(bearer()),
                    StatusCode::NOT_FOUND,
                ),
                (
                    "PUT",
                    format!("{path}/extra"),
                    None,
                    StatusCode::METHOD_NOT_ALLOWED,
                ),
                ("GET", format!("{path}x"), None, StatusCode::NOT_FOUND),
                ("POST", path.clone(), None, StatusCode::METHOD_NOT_ALLOWED),
            ] {
                let (got, headers, body) =
                    send_request(&app, method, &uri, None, auth.as_deref(), None).await;
                assert_eq!(got, status, "{method} {uri}");
                assert_ticket_headers(&headers);
                bodies.lock().unwrap().push(body);
            }
            active.store(false, std::sync::atomic::Ordering::SeqCst);
            let (status, _, body) = play(&app, &path, None).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            bodies.lock().unwrap().push(body);
        }
    })
    .await;

    assert!(
        logs.contains("path=/v1/media-stream/{ticket}"),
        "the request log records the template: {logs}"
    );
    assert!(
        logs.contains("stream ticket refused: its token is revoked or expired"),
        "the refusal is logged: {logs}"
    );
    assert!(
        logs.contains("stream ticket refused: unknown"),
        "an unknown ticket is logged, without itself: {logs}"
    );
    assert!(!logs.contains(&ticket), "a ticket reached the log: {logs}");
    for body in bodies.lock().unwrap().iter() {
        let text = String::from_utf8_lossy(body);
        assert!(!text.contains(&ticket), "an error body repeats it: {text}");
    }
    fx.store.delete_account_row(fx.account_id).await.unwrap();
}
