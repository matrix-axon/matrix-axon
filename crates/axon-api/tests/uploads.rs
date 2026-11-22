//! Handler tests for the M15a staged media-upload routes.

mod common;

use std::sync::Arc;

use axon_api::{AppState, StagedUploadService};
use axon_store::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    StubDeviceList, StubLifecycle, StubMediaProxy, StubSender, StubTokenVerifier, StubTrust,
    StubUploads, StubVerification, UploadOutcome, TEST_TOKEN,
};
use serde_json::Value;
use tower::ServiceExt;
use uuid::Uuid;

async fn store() -> Store {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    Store::connect(&url, 5).await.expect("connect + migrate")
}

fn app(store: Store, uploads: Arc<dyn StagedUploadService>) -> axum::Router {
    let (live, _rx) = tokio::sync::broadcast::channel(16);
    let sender = Arc::new(StubSender::ok("$unused:localhost"));
    let lifecycle = Arc::new(StubLifecycle::ok(Uuid::nil()));
    let verify = Arc::new(StubVerification::ok("$unused-flow"));
    let trust = Arc::new(StubTrust::ok());
    let devices = Arc::new(StubDeviceList::ok());
    let verifier = Arc::new(StubTokenVerifier::ok());
    let media = Arc::new(StubMediaProxy);
    axon_api::router(
        AppState::new(
            store, live, sender, lifecycle, verify, trust, devices, verifier, media, None,
        )
        .with_staged_uploads(uploads),
    )
}

async fn request(
    app: &axum::Router,
    method: &str,
    uri: &str,
    content_type: Option<&str>,
    body: Body,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TEST_TOKEN}"));
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .expect("request");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, json)
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn stage_upload_streams_body_and_normalizes_metadata() {
    let store = store().await;
    let uploads = Arc::new(StubUploads::ok());
    let app = app(store, uploads.clone());
    let account_id = Uuid::new_v4();

    let (status, body) = request(
        &app,
        "POST",
        &format!(
            "/v1/accounts/{account_id}/media/uploads?kind=image&filename=C:%5Ctmp%5Cphoto.png"
        ),
        Some("image/png"),
        Body::from("abc"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["kind"], "image");
    assert_eq!(body["data"]["filename"], "photo.png");
    assert_eq!(body["data"]["content_type"], "image/png");
    assert_eq!(body["data"]["size_bytes"], 3);

    let calls = uploads.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].account_id, account_id);
    assert_eq!(calls[0].kind, axon_api::MediaUploadKindDto::Image);
    assert_eq!(calls[0].filename, "photo.png");
    assert_eq!(calls[0].content_type.as_deref(), Some("image/png"));
    assert_eq!(calls[0].bytes, b"abc");
}

/// ADR 0142 section 1: staging accepts `video` and `audio`, passes the kind
/// and declared type through unchanged, and still refuses an unknown kind.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn stage_upload_accepts_video_and_audio_kinds() {
    let store = store().await;
    let uploads = Arc::new(StubUploads::ok());
    let app = app(store, uploads.clone());
    let account_id = Uuid::new_v4();

    for (query, content_type, kind) in [
        (
            "kind=video&filename=IMG_9306.mov",
            "video/quicktime",
            axon_api::MediaUploadKindDto::Video,
        ),
        (
            "kind=audio&filename=memo.ogg",
            "audio/ogg",
            axon_api::MediaUploadKindDto::Audio,
        ),
    ] {
        let (status, body) = request(
            &app,
            "POST",
            &format!("/v1/accounts/{account_id}/media/uploads?{query}"),
            Some(content_type),
            Body::from("abc"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["kind"], kind.as_str());
        assert_eq!(body["data"]["content_type"], content_type);
        let last = uploads.calls().pop().expect("a staging call");
        assert_eq!(last.kind, kind);
        assert_eq!(last.content_type.as_deref(), Some(content_type));
    }

    let (status, _) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/media/uploads?kind=sticker&filename=x"),
        Some("image/png"),
        Body::from("abc"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(uploads.calls().len(), 2);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn delete_upload_routes_to_service() {
    let store = store().await;
    let uploads = Arc::new(StubUploads::ok());
    let app = app(store, uploads.clone());
    let account_id = Uuid::new_v4();
    let upload_id = Uuid::new_v4();

    let (status, body) = request(
        &app,
        "DELETE",
        &format!("/v1/accounts/{account_id}/media/uploads/{upload_id}"),
        None,
        Body::empty(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], serde_json::json!({}));
    assert_eq!(uploads.deletes(), vec![(account_id, upload_id)]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn upload_errors_map_to_http_statuses() {
    let store = store().await;
    let uploads = Arc::new(StubUploads::failing(UploadOutcome::TooLarge { cap: 2 }));
    let app = app(store, uploads);
    let account_id = Uuid::new_v4();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/v1/accounts/{account_id}/media/uploads?kind=file&filename=file.bin"),
        Some("application/octet-stream"),
        Body::from("abc"),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], "payload_too_large");
}
