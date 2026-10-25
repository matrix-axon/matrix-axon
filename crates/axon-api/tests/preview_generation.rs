//! Explicit generation, account isolation and native recovery wire contracts.
mod common;
use axon_api::{
    ApiError, AppState, GeneratedPreviewDto, PreviewGenerationStatus, PreviewGenerator,
};
use axon_store::{AccountState, Store};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use common::{
    StubDeviceList, StubLifecycle, StubMediaProxy, StubSender, StubTokenVerifier, StubTrust,
    StubVerification, TEST_TOKEN,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;
use uuid::Uuid;

struct Generator(AtomicUsize);
#[async_trait::async_trait]
impl PreviewGenerator for Generator {
    async fn generate(&self, _: Uuid, _: String) -> Result<GeneratedPreviewDto, ApiError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(GeneratedPreviewDto::fallback(
            PreviewGenerationStatus::Disabled,
        ))
    }
}
fn app(store: Store, generator: Arc<Generator>) -> axum::Router {
    let (live, _) = tokio::sync::broadcast::channel(16);
    axon_api::router(
        AppState::new(
            store,
            live,
            Arc::new(StubSender::ok("$unused:localhost")),
            Arc::new(StubLifecycle::ok(Uuid::nil())),
            Arc::new(StubVerification::ok("$flow")),
            Arc::new(StubTrust::ok()),
            Arc::new(StubDeviceList::ok()),
            Arc::new(StubTokenVerifier::ok()),
            Arc::new(StubMediaProxy),
            None,
        )
        .with_preview_generator(generator),
    )
}
async fn send(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: &str,
    auth: bool,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if auth {
        req = req.header("authorization", format!("Bearer {TEST_TOKEN}"));
    }
    let res = app
        .clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
#[ignore = "requires Postgres"]
async fn generation_requires_explicit_authenticated_valid_account_request() {
    let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
        .await
        .unwrap();
    let account = store
        .upsert_account(
            &format!("@preview-{}:localhost", Uuid::new_v4()),
            "https://hs.example.org",
        )
        .await
        .unwrap();
    let generator = Arc::new(Generator(AtomicUsize::new(0)));
    let router = app(store.clone(), generator.clone());
    let path = format!("/v1/accounts/{}/link-previews/generate", account.account_id);
    for (method, uri, body, auth, expected) in [
        ("GET", "/v1/accounts".to_string(), "", true, StatusCode::OK),
        (
            "POST",
            path.clone(),
            r#"{"url":"https://example.org"}"#,
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "POST",
            path.clone(),
            r#"{"url":"http://127.1"}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            path.clone(),
            r#"{"url":"https://example.org","image":"https://bad"}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            format!("/v1/accounts/{}/link-previews/generate", Uuid::new_v4()),
            r#"{"url":"https://example.org"}"#,
            true,
            StatusCode::NOT_FOUND,
        ),
    ] {
        assert_eq!(send(&router, method, &uri, body, auth).await.0, expected);
    }
    assert_eq!(generator.0.load(Ordering::SeqCst), 0);
    let (status, body) = send(
        &router,
        "POST",
        &path,
        r#"{"url":"https://example.org"}"#,
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "disabled");
    assert_eq!(generator.0.load(Ordering::SeqCst), 1);
    store
        .set_account_state(account.account_id, AccountState::Deleting)
        .await
        .unwrap();
    assert_eq!(
        send(
            &router,
            "POST",
            &path,
            r#"{"url":"https://example.org"}"#,
            true
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(generator.0.load(Ordering::SeqCst), 1);
    let recovery = format!(
        "/v1/accounts/{}/media/uploads/{}/preview",
        account.account_id,
        Uuid::new_v4()
    );
    let (status, body) = send(&router, "GET", &recovery, "", true).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    store.delete_account_row(account.account_id).await.unwrap();
}

#[test]
fn native_preview_generation_contract_fixture_matches_dtos_and_openapi() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/preview-generation-contract.json")).unwrap();
    use utoipa::OpenApi;
    let spec = serde_json::to_value(axon_api::ApiDoc::openapi()).unwrap();
    assert_eq!(
        fixture["thumbnail_recovery"]["max_bytes"],
        axon_core::link_preview::MAX_IMAGE_BYTES
    );
    assert_eq!(
        fixture["thumbnail_recovery"]["max_pixels"],
        axon_core::link_preview::MAX_IMAGE_PIXELS
    );
    let recovery = &spec["paths"][fixture["thumbnail_recovery"]["path"].as_str().unwrap()]["get"];
    for code in ["200", "400", "404", "413", "503"] {
        assert!(recovery["responses"].get(code).is_some());
    }
    assert_eq!(
        fixture["thumbnail_recovery"]["automatic_regeneration"],
        false
    );
    let _: axon_api::GeneratePreviewRequest =
        serde_json::from_value(fixture["request"].clone()).unwrap();
    let statuses = spec["components"]["schemas"]["PreviewGenerationStatus"]["enum"]
        .as_array()
        .unwrap();
    for case in fixture["outcomes"].as_array().unwrap() {
        let dto: GeneratedPreviewDto = serde_json::from_value(case["data"].clone()).unwrap();
        assert!(statuses.contains(&case["data"]["status"]));
        assert_eq!(serde_json::to_value(&dto).unwrap(), case["data"]);
        if let Some(preview) = dto.preview {
            let metadata = axon_core::link_preview::PreviewMetadata {
                matched_url: preview.matched_url.clone(),
                title: preview.title,
                description: preview.description,
                site_name: preview.site_name,
            };
            axon_core::link_preview::validate_metadata(&preview.matched_url, &[metadata]).unwrap();
        }
    }
}
