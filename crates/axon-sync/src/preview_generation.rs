//! Bounded authenticated acquisition via the configured homeserver only.
//! Never contacts the destination webpage or an arbitrary OG image URL.
use crate::manager::ClientManager;
use axon_core::{
    link_preview::{PreviewMetadata, MAX_IMAGE_BYTES, MAX_MXC_LENGTH},
    preview_generation::{self, PreviewUrl},
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;
use tracing::Instrument;
use url::Url;
use uuid::Uuid;

const RESPONSE_CAP: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewFailure {
    Unsupported,
    Disabled,
    Timeout,
    Unavailable,
    RateLimited,
}
pub struct AcquiredPreview {
    pub metadata: PreviewMetadata,
    pub image: Option<Vec<u8>>,
    pub image_unavailable: bool,
}
#[derive(Clone)]
pub struct HomeserverPreviewGenerator {
    manager: ClientManager,
    http: Client,
}
impl HomeserverPreviewGenerator {
    pub(crate) fn new(manager: ClientManager) -> Result<Self, reqwest::Error> {
        Ok(Self {
            manager,
            http: crate::client::preview_http_client()?,
        })
    }
    pub fn acquire<'a>(
        &'a self,
        account_id: Uuid,
        url: &'a PreviewUrl,
    ) -> futures_util::future::BoxFuture<'a, Result<AcquiredPreview, PreviewFailure>> {
        // Keep the SDK connection future behind this crate's boundary, like
        // the other boxed async ports; consumers need not re-prove its Send.
        Box::pin(
            async move {
                // Connection establishment plus all requests share a single deadline.
                tokio::time::timeout(Duration::from_secs(25), async {
                    let client = self
                        .manager
                        .get_or_connect(account_id)
                        .await
                        .map_err(|_| unavailable("connect", "account_connection"))?;
                    acquire_from(&self.http, &client.homeserver(), &client, url).await
                })
                .await
                .map_err(|_| {
                    tracing::debug!(
                        error_kind = "acquisition_deadline",
                        "preview acquisition timed out"
                    );
                    PreviewFailure::Timeout
                })?
            }
            .instrument(tracing::debug_span!("preview_acquisition", %account_id)),
        )
    }
}

fn endpoint(hs: &Url, path: &[&str]) -> Result<Url, PreviewFailure> {
    let mut url = hs.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .map_err(|_| PreviewFailure::Unavailable)?
        .pop_if_empty()
        .extend(path);
    Ok(url)
}
// The SDK's normal send/media path collects the entire body before exposing a
// response. Keep this stream boundary (including no redirects), but delegate
// refresh, serialization of rotation and persistence notifications to the SDK.
#[async_trait::async_trait]
trait PreviewCredentials: Send + Sync {
    fn token(&self) -> Option<String>;
    fn refreshable(&self) -> bool;
    async fn refresh(&self) -> Result<(), ()>;
}
#[async_trait::async_trait]
impl PreviewCredentials for matrix_sdk::Client {
    fn token(&self) -> Option<String> {
        self.access_token()
    }
    fn refreshable(&self) -> bool {
        self.session_tokens()
            .is_some_and(|s| s.refresh_token.is_some())
    }
    async fn refresh(&self) -> Result<(), ()> {
        let client = self.clone();
        // Keep SDK token rotation/persistence alive if the preview's shorter
        // deadline expires after the issuer has accepted the refresh token.
        tokio::spawn(
            async move {
                tokio::time::timeout(Duration::from_secs(30), client.refresh_access_token())
                    .await
                    .map_err(|_| ())?
                    .map_err(|_| ())
            }
            .instrument(tracing::Span::current()),
        )
        .await
        .map_err(|_| ())?
    }
}
fn unavailable(phase: &'static str, reason: &'static str) -> PreviewFailure {
    tracing::debug!(
        phase,
        error_kind = reason,
        "homeserver preview acquisition unavailable"
    );
    PreviewFailure::Unavailable
}
async fn read_response(
    mut response: reqwest::Response,
    cap: usize,
    phase: &'static str,
) -> Result<Vec<u8>, PreviewFailure> {
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(unavailable(phase, "response_too_large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| transport(e, phase))? {
        if chunk.len() > cap.saturating_sub(bytes.len()) {
            return Err(unavailable(phase, "response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn request(
    http: &Client,
    url: Url,
    auth: &dyn PreviewCredentials,
    cap: usize,
    phase: &'static str,
) -> Result<Vec<u8>, PreviewFailure> {
    for attempt in 0..=1 {
        let token = auth
            .token()
            .ok_or_else(|| unavailable(phase, "authentication_missing"))?;
        let response = http
            .get(url.clone())
            .bearer_auth(&token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| transport(e, phase))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            let bytes = read_response(response, RESPONSE_CAP, phase).await?;
            let unknown = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("errcode")
                        .and_then(Value::as_str)
                        .map(|s| s == "M_UNKNOWN_TOKEN")
                })
                .unwrap_or(false);
            if attempt == 0 && unknown && auth.refreshable() {
                // Reuse an intervening SDK rotation. Otherwise its refresh lock
                // coalesces concurrent requests and its normal watcher persists it.
                if auth.token().as_deref() == Some(token.as_str()) {
                    tokio::time::timeout(REQUEST_TIMEOUT, auth.refresh())
                        .await
                        .map_err(|_| {
                            tracing::debug!(
                                phase,
                                error_kind = "refresh_timeout",
                                "preview refresh timed out"
                            );
                            PreviewFailure::Timeout
                        })?
                        .map_err(|_| unavailable(phase, "refresh_failed"))?;
                }
                tracing::debug!(phase, "retrying preview request after SDK token refresh");
                continue;
            }
            return Err(unavailable(phase, "authentication_rejected"));
        }
        tracing::debug!(
            phase,
            http_status = status.as_u16(),
            "homeserver preview response"
        );
        match status {
            StatusCode::FORBIDDEN => return Err(unavailable(phase, "forbidden")),
            StatusCode::TOO_MANY_REQUESTS => return Err(PreviewFailure::RateLimited),
            StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED => {
                return Err(PreviewFailure::Unsupported)
            }
            status if !status.is_success() => return Err(unavailable(phase, "http_status")),
            _ => (),
        }
        return read_response(response, cap, phase).await;
    }
    Err(unavailable(phase, "authentication_rejected"))
}
fn transport(error: reqwest::Error, phase: &'static str) -> PreviewFailure {
    if error.is_timeout() {
        tracing::debug!(
            phase,
            error_kind = "request_timeout",
            "preview request timed out"
        );
        PreviewFailure::Timeout
    } else {
        unavailable(phase, "transport")
    }
}
async fn acquire_from(
    http: &Client,
    hs: &Url,
    auth: &dyn PreviewCredentials,
    url: &PreviewUrl,
) -> Result<AcquiredPreview, PreviewFailure> {
    // New capability is optional on older servers. An explicit false stops
    // before preview_url; every other probe outcome retains compatibility.
    let caps = endpoint(hs, &["_matrix", "client", "v3", "capabilities"])?;
    match request(http, caps, auth, RESPONSE_CAP, "capabilities").await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => {
                if value.pointer("/capabilities/m.preview_url/enabled") == Some(&Value::Bool(false))
                {
                    return Err(PreviewFailure::Disabled);
                }
                if !value.get("capabilities").is_some_and(Value::is_object)
                    || value
                        .pointer("/capabilities/m.preview_url/enabled")
                        .is_some_and(|v| !v.is_boolean())
                {
                    tracing::debug!(
                        phase = "capabilities",
                        error_kind = "capability_shape",
                        "ignoring malformed optional preview capabilities"
                    );
                }
            }
            Err(_) => {
                tracing::debug!(
                    phase = "capabilities",
                    error_kind = "capability_json",
                    "ignoring malformed optional preview capabilities"
                );
            }
        },
        Err(_) => tracing::debug!(
            phase = "capabilities",
            "optional preview capability probe failed; continuing"
        ),
    }
    let mut target = endpoint(hs, &["_matrix", "client", "v1", "media", "preview_url"])?;
    target.query_pairs_mut().append_pair("url", url.as_str());
    let bytes = request(http, target, auth, RESPONSE_CAP, "metadata").await?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| unavailable("metadata", "metadata_json"))?;
    let metadata = preview_generation::normalize(url, &value)
        .ok_or_else(|| unavailable("metadata", "metadata_shape"))?;
    let mut result = AcquiredPreview {
        metadata,
        image: None,
        image_unavailable: false,
    };
    if let Some(image) = value.get("og:image").filter(|v| !v.is_null()) {
        result.image_unavailable = true;
        if let Some(mxc) = image.as_str().filter(|s| s.len() <= MAX_MXC_LENGTH) {
            if let Ok((server, id)) = <&matrix_sdk::ruma::MxcUri>::from(mxc).parts() {
                let mut target = endpoint(
                    hs,
                    &[
                        "_matrix",
                        "client",
                        "v1",
                        "media",
                        "download",
                        server.as_str(),
                        id,
                    ],
                )?;
                target
                    .query_pairs_mut()
                    .append_pair("allow_redirect", "false")
                    .append_pair("timeout_ms", "5000");
                if let Ok(bytes) =
                    request(http, target, auth, MAX_IMAGE_BYTES as usize, "thumbnail").await
                {
                    result.image = Some(bytes);
                    result.image_unavailable = false;
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        extract::{Query, State},
        http::{header, HeaderMap},
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct FixedCredentials;
    #[async_trait::async_trait]
    impl PreviewCredentials for FixedCredentials {
        fn token(&self) -> Option<String> {
            Some("matrix-test-token".into())
        }
        fn refreshable(&self) -> bool {
            false
        }
        async fn refresh(&self) -> Result<(), ()> {
            panic!("fixed credentials must not refresh")
        }
    }
    fn preview_url() -> PreviewUrl {
        PreviewUrl::parse("https://example.org/private?value=1".into()).unwrap()
    }
    #[derive(Clone)]
    struct Fixture {
        mode: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }
    async fn capabilities(State(f): State<Fixture>, headers: HeaderMap) -> Response {
        assert_eq!(headers[header::AUTHORIZATION], "Bearer matrix-test-token");
        if f.mode == "production_http" {
            assert_eq!(headers[header::USER_AGENT], crate::client::user_agent());
        }
        f.calls.lock().unwrap().push("capabilities".into());
        if f.mode == "disabled" {
            Json(serde_json::json!({"capabilities":{"m.preview_url":{"enabled":false}}}))
                .into_response()
        } else if let Some(mode) = f.mode.strip_prefix("caps_") {
            match mode {
                "forbidden" => StatusCode::FORBIDDEN.into_response(),
                "limited" => StatusCode::TOO_MANY_REQUESTS.into_response(),
                "unsupported" => StatusCode::NOT_FOUND.into_response(),
                "unimplemented" => StatusCode::NOT_IMPLEMENTED.into_response(),
                "fail" => StatusCode::BAD_GATEWAY.into_response(),
                "auth" => (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"errcode":"M_UNKNOWN_TOKEN"})),
                )
                    .into_response(),
                "malformed" => Body::from("not json").into_response(),
                "shape" => {
                    Json(serde_json::json!({"capabilities":{"m.preview_url":{"enabled":"false"}}}))
                        .into_response()
                }
                "absent" => Json(serde_json::json!({})).into_response(),
                "large" => Body::from("x".repeat(RESPONSE_CAP + 1)).into_response(),
                "redirect" => {
                    (StatusCode::FOUND, [(header::LOCATION, "/must-not-follow")]).into_response()
                }
                "timeout" => {
                    tokio::time::sleep(Duration::from_secs(9)).await;
                    Json(serde_json::json!({})).into_response()
                }
                _ => panic!("unknown capabilities fixture"),
            }
        } else {
            Json(serde_json::json!({"capabilities":{}})).into_response()
        }
    }
    async fn preview(
        State(f): State<Fixture>,
        Query(q): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Response {
        assert_eq!(headers[header::AUTHORIZATION], "Bearer matrix-test-token");
        assert_eq!(q["url"], "https://example.org/private?value=1");
        f.calls.lock().unwrap().push("preview".into());
        match f.mode {
            "unsupported" => StatusCode::NOT_FOUND.into_response(),
            "forbidden" => StatusCode::FORBIDDEN.into_response(),
            "limited" => StatusCode::TOO_MANY_REQUESTS.into_response(),
            "fail" => StatusCode::BAD_GATEWAY.into_response(),
            "redirect" => (StatusCode::FOUND, [(header::LOCATION, "/must-not-follow")]).into_response(),
            "timeout" => { tokio::time::sleep(Duration::from_secs(9)).await; Json(serde_json::json!({})).into_response() },
            "large" => Body::from("x".repeat(RESPONSE_CAP + 1)).into_response(),
            "chunked" => Body::from_stream(futures_util::stream::iter((0..80).map(|_| Ok::<_, std::io::Error>(vec![b'x';1024])))).into_response(),
            "malformed" => Body::from("not json").into_response(),
            "remote_image" => Json(serde_json::json!({"og:title":"Useful title", "og:image":format!("http://{}/must-not-follow", headers[header::HOST].to_str().unwrap())})).into_response(),
            "bad_mxc" => Json(serde_json::json!({"og:title":"Useful title", "og:image":"mxc://hs/../capabilities"})).into_response(),
            _ => Json(serde_json::json!({"og:title":"Useful title", "og:image":"mxc://hs/image", "og:image:width":999999999999_u64,"matrix:image:encrypted":{"key":"ignored"}})).into_response(),
        }
    }
    async fn image(State(f): State<Fixture>, headers: HeaderMap) -> Response {
        assert_eq!(headers[header::AUTHORIZATION], "Bearer matrix-test-token");
        f.calls.lock().unwrap().push("image".into());
        match f.mode {
            "missing_image" => StatusCode::NOT_FOUND.into_response(),
            "large_image" => Body::from(vec![0; MAX_IMAGE_BYTES as usize + 1]).into_response(),
            _ => Body::from("thumbnail bytes").into_response(),
        }
    }
    async fn unexpected(State(f): State<Fixture>) -> Response {
        f.calls.lock().unwrap().push("unexpected".into());
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
    async fn fixture(mode: &'static str) -> (Url, Fixture, tokio::task::JoinHandle<()>) {
        let f = Fixture {
            mode,
            calls: Default::default(),
        };
        let app = Router::new()
            .route("/_matrix/client/v3/capabilities", get(capabilities))
            .route("/_matrix/client/v1/media/preview_url", get(preview))
            .route("/_matrix/client/v1/media/download/hs/image", get(image))
            .fallback(unexpected)
            .with_state(f.clone());
        let app = if mode == "prefixed" {
            Router::new().nest("/matrix", app)
        } else {
            app
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut url = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        if mode == "prefixed" {
            url.set_path("/matrix/");
        }
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, f, task)
    }
    #[test]
    fn preview_generation_endpoints_preserve_configured_base_path() {
        for base in ["https://hs.example.org", "https://hs.example.org/"] {
            assert_eq!(
                endpoint(
                    &Url::parse(base).unwrap(),
                    &["_matrix", "client", "v3", "capabilities"]
                )
                .unwrap()
                .as_str(),
                "https://hs.example.org/_matrix/client/v3/capabilities"
            );
        }
        for base in [
            "https://hs.example.org/matrix%20gateway",
            "https://hs.example.org/matrix%20gateway/?old=query#fragment",
        ] {
            assert_eq!(
                endpoint(
                    &Url::parse(base).unwrap(),
                    &["_matrix", "client", "v3", "capabilities"]
                )
                .unwrap()
                .as_str(),
                "https://hs.example.org/matrix%20gateway/_matrix/client/v3/capabilities"
            );
        }
    }

    #[tokio::test]
    async fn preview_generation_transport_uses_the_production_user_agent() {
        let (hs, f, task) = fixture("production_http").await;
        let result = acquire_from(
            &crate::client::preview_http_client().unwrap(),
            &hs,
            &FixedCredentials,
            &preview_url(),
        )
        .await
        .unwrap();
        assert!(result.image.is_some());
        assert_eq!(
            *f.calls.lock().unwrap(),
            vec!["capabilities", "preview", "image"]
        );
        task.abort();
    }
    #[tokio::test]
    async fn preview_generation_optional_capabilities_fail_open_except_explicit_false() {
        for mode in [
            "caps_forbidden",
            "caps_limited",
            "caps_unsupported",
            "caps_unimplemented",
            "caps_fail",
            "caps_auth",
            "caps_malformed",
            "caps_shape",
            "caps_absent",
            "caps_large",
            "caps_redirect",
            "caps_timeout",
        ] {
            let (hs, f, task) = fixture(mode).await;
            let http = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(150))
                .build()
                .unwrap();
            let result = acquire_from(&http, &hs, &FixedCredentials, &preview_url())
                .await
                .unwrap();
            assert_eq!(
                result.metadata.title.as_deref(),
                Some("Useful title"),
                "{mode}"
            );
            assert!(result.image.is_some(), "{mode}");
            assert_eq!(
                *f.calls.lock().unwrap(),
                vec!["capabilities", "preview", "image"],
                "{mode}"
            );
            task.abort();
        }
    }
    #[tokio::test]
    async fn preview_generation_boundaries_and_capability_policy() {
        for (mode, expected) in [
            ("disabled", PreviewFailure::Disabled),
            ("unsupported", PreviewFailure::Unsupported),
            ("forbidden", PreviewFailure::Unavailable),
            ("fail", PreviewFailure::Unavailable),
            ("limited", PreviewFailure::RateLimited),
            ("redirect", PreviewFailure::Unavailable),
            ("large", PreviewFailure::Unavailable),
            ("chunked", PreviewFailure::Unavailable),
            ("malformed", PreviewFailure::Unavailable),
            ("timeout", PreviewFailure::Timeout),
        ] {
            let (hs, f, task) = fixture(mode).await;
            let http = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(150))
                .build()
                .unwrap();
            assert!(
                matches!(acquire_from(&http, &hs, &FixedCredentials, &preview_url()).await, Err(e) if e == expected),
                "{mode}"
            );
            assert!(
                !f.calls.lock().unwrap().contains(&"unexpected".into()),
                "followed a redirect for {mode}"
            );
            if mode == "disabled" {
                assert_eq!(*f.calls.lock().unwrap(), vec!["capabilities"]);
            }
            task.abort();
        }
    }
    #[tokio::test]
    async fn preview_generation_uses_only_homeserver_mxc_and_retains_text_when_image_fails() {
        for mode in [
            "ok",
            "prefixed",
            "missing_image",
            "large_image",
            "remote_image",
            "bad_mxc",
        ] {
            let (hs, f, task) = fixture(mode).await;
            let http = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let result = acquire_from(&http, &hs, &FixedCredentials, &preview_url())
                .await
                .unwrap();
            assert_eq!(result.metadata.title.as_deref(), Some("Useful title"));
            assert!(
                !f.calls.lock().unwrap().contains(&"unexpected".into()),
                "fetched an ineligible image for {mode}"
            );
            assert_eq!(result.image_unavailable, !matches!(mode, "ok" | "prefixed"));
            assert_eq!(
                result.image.as_deref(),
                if matches!(mode, "ok" | "prefixed") {
                    Some(b"thumbnail bytes".as_slice())
                } else {
                    None
                }
            );
            assert_eq!(
                f.calls.lock().unwrap().contains(&"image".into()),
                !matches!(mode, "remote_image" | "bad_mxc")
            );
            task.abort();
        }
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use axum::{
        extract::{Path, State},
        http::HeaderMap,
        response::{IntoResponse, Response},
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    #[derive(Clone)]
    struct Fixture {
        base: String,
        phase: &'static str,
        refreshes: Arc<AtomicUsize>,
        rejections: Arc<AtomicUsize>,
    }
    async fn metadata(State(f): State<Fixture>) -> Json<Value> {
        Json(
            serde_json::json!({"issuer":format!("{}/",f.base),"authorization_endpoint":format!("{}/authorize",f.base),"token_endpoint":format!("{}/token",f.base),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"revocation_endpoint":format!("{}/revoke",f.base),"code_challenge_methods_supported":["S256"]}),
        )
    }
    async fn token(State(f): State<Fixture>) -> Response {
        f.refreshes.fetch_add(1, Ordering::SeqCst);
        if f.phase == "owned" {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if f.phase == "refresh_denied" {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error":"invalid_grant"})),
            )
                .into_response();
        }
        Json(serde_json::json!({"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":300})).into_response()
    }
    async fn resource(
        State(f): State<Fixture>,
        Path(phase): Path<String>,
        headers: HeaderMap,
    ) -> Response {
        let old = headers
            .get("authorization")
            .is_some_and(|v| v == "Bearer old-access");
        if (f.phase == phase && old)
            || (phase == "preview_url"
                && matches!(
                    f.phase,
                    "refresh_denied" | "retry_rejected" | "generic401" | "legacy"
                ))
        {
            f.rejections.fetch_add(1, Ordering::SeqCst);
            return (StatusCode::UNAUTHORIZED,Json(serde_json::json!({"errcode":if f.phase=="generic401" {"M_UNAUTHORIZED"}else{"M_UNKNOWN_TOKEN"}}))).into_response();
        }
        match phase.as_str() {
            "capabilities" => Json(serde_json::json!({"capabilities":{}})).into_response(),
            "preview_url" => {
                Json(serde_json::json!({"og:title":"After refresh","og:image":"mxc://hs/image"}))
                    .into_response()
            }
            _ => axum::body::Body::from("thumbnail bytes").into_response(),
        }
    }
    #[tokio::test]
    async fn preview_generation_uses_sdk_oauth_refresh_at_every_phase_and_retries_once() {
        for phase in [
            "owned",
            "capabilities",
            "preview_url",
            "image",
            "refresh_denied",
            "retry_rejected",
            "generic401",
            "legacy",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let f = Fixture {
                base: format!("http://{}", listener.local_addr().unwrap()),
                phase,
                refreshes: Default::default(),
                rejections: Default::default(),
            };
            let app = Router::new()
                .route(
                    "/_matrix/client/versions",
                    get(|| async {
                        Json(serde_json::json!({"versions":["v1.15"],"unstable_features":{}}))
                    }),
                )
                .route("/_matrix/client/v1/auth_metadata", get(metadata))
                .route(
                    "/_matrix/client/unstable/org.matrix.msc2965/auth_metadata",
                    get(metadata),
                )
                .route("/token", post(token))
                .route("/_matrix/client/v3/{phase}", get(resource))
                .route("/_matrix/client/v1/media/{phase}", get(resource))
                .route(
                    "/_matrix/client/v1/media/download/hs/{phase}",
                    get(resource),
                )
                .with_state(f.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client = matrix_sdk::Client::builder()
                .homeserver_url(&f.base)
                .handle_refresh_tokens()
                .build()
                .await
                .unwrap();
            client
                .oauth()
                .restore_session(
                    matrix_sdk::authentication::oauth::OAuthSession {
                        client_id: matrix_sdk::authentication::oauth::ClientId::new(
                            "test-client".into(),
                        ),
                        user: matrix_sdk::authentication::oauth::UserSession {
                            meta: matrix_sdk::SessionMeta {
                                user_id: "@test:localhost".try_into().unwrap(),
                                device_id: "PREVIEW".into(),
                            },
                            tokens: matrix_sdk::SessionTokens {
                                access_token: "old-access".into(),
                                refresh_token: if phase == "legacy" {
                                    None
                                } else {
                                    Some("old-refresh".into())
                                },
                            },
                        },
                    },
                    matrix_sdk::store::RoomLoadSettings::default(),
                )
                .await
                .unwrap();
            if phase == "owned" {
                let rotating = client.clone();
                let waiter =
                    tokio::spawn(async move { PreviewCredentials::refresh(&rotating).await });
                tokio::time::timeout(Duration::from_secs(2), async {
                    while f.refreshes.load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                waiter.abort();
                tokio::time::timeout(Duration::from_secs(2), async {
                    while client.access_token().as_deref() != Some("new-access") {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("SDK rotation must finish after the preview waiter disappears");
                assert_eq!(f.refreshes.load(Ordering::SeqCst), 1);
                server.abort();
                continue;
            }
            let result = acquire_from(
                &crate::client::preview_http_client().unwrap(),
                &client.homeserver(),
                &client,
                &PreviewUrl::parse("https://example.org".into()).unwrap(),
            )
            .await;
            if matches!(
                phase,
                "refresh_denied" | "retry_rejected" | "generic401" | "legacy"
            ) {
                assert!(
                    matches!(result, Err(PreviewFailure::Unavailable)),
                    "{phase}"
                );
            } else {
                let result = result.unwrap();
                assert_eq!(result.metadata.title.as_deref(), Some("After refresh"));
                assert!(result.image.is_some(), "{phase}");
                assert!(client.session_tokens().unwrap().access_token == "new-access");
                assert!(
                    client.session_tokens().unwrap().refresh_token.as_deref()
                        == Some("new-refresh")
                );
            }
            assert_eq!(
                f.refreshes.load(Ordering::SeqCst),
                usize::from(!matches!(phase, "generic401" | "legacy")),
                "{phase}"
            );
            assert_eq!(
                f.rejections.load(Ordering::SeqCst),
                if phase == "retry_rejected" { 2 } else { 1 },
                "{phase}"
            );
            server.abort();
        }
    }
}
