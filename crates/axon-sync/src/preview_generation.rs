//! Bounded authenticated acquisition via the configured homeserver only.
//! Never contacts the destination webpage or an arbitrary OG image URL.
use crate::manager::ClientManager;
use axon_core::{
    link_preview::{PreviewMetadata, MAX_IMAGE_BYTES, MAX_MXC_LENGTH},
    preview_generation,
};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use std::time::Duration;
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
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(4))
                .timeout(REQUEST_TIMEOUT)
                .build()?,
        })
    }
    pub fn acquire<'a>(
        &'a self,
        account_id: Uuid,
        url: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<AcquiredPreview, PreviewFailure>> {
        // Keep the SDK connection future behind this crate's boundary, like
        // the other boxed async ports; consumers need not re-prove its Send.
        Box::pin(async move {
            if !preview_generation::eligible_url(url) {
                return Err(PreviewFailure::Unavailable);
            }
            // Connection establishment plus all requests share a single deadline.
            tokio::time::timeout(Duration::from_secs(25), async {
                let client = self
                    .manager
                    .get_or_connect(account_id)
                    .await
                    .map_err(|_| PreviewFailure::Unavailable)?;
                let token = client.access_token().ok_or(PreviewFailure::Unavailable)?;
                acquire_from(&self.http, &client.homeserver(), &token, url).await
            })
            .await
            .map_err(|_| PreviewFailure::Timeout)?
        })
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
async fn request(
    http: &Client,
    url: Url,
    token: &str,
    cap: usize,
) -> Result<Vec<u8>, PreviewFailure> {
    // URLs are constructed from the configured homeserver, never OG data.
    // Neither raw reqwest errors nor response bodies escape into diagnostics.
    let mut response = http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(transport)?;
    match response.status() {
        StatusCode::FORBIDDEN => return Err(PreviewFailure::Disabled),
        StatusCode::NOT_FOUND | StatusCode::NOT_IMPLEMENTED => {
            return Err(PreviewFailure::Unsupported)
        }
        status if !status.is_success() => return Err(PreviewFailure::Unavailable),
        _ => (),
    }
    if response
        .content_length()
        .is_some_and(|len| len > cap as u64)
    {
        return Err(PreviewFailure::Unavailable);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if chunk.len() > cap.saturating_sub(bytes.len()) {
            return Err(PreviewFailure::Unavailable);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
fn transport(error: reqwest::Error) -> PreviewFailure {
    if error.is_timeout() {
        PreviewFailure::Timeout
    } else {
        PreviewFailure::Unavailable
    }
}
async fn acquire_from(
    http: &Client,
    hs: &Url,
    token: &str,
    url: &str,
) -> Result<AcquiredPreview, PreviewFailure> {
    // New capability is optional on older servers. An explicit false stops
    // before preview_url; absent/unsupported capabilities retain compatibility.
    let caps = endpoint(hs, &["_matrix", "client", "v3", "capabilities"])?;
    match request(http, caps, token, RESPONSE_CAP).await {
        Ok(bytes) => {
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| PreviewFailure::Unavailable)?;
            if !value.get("capabilities").is_some_and(Value::is_object)
                || value
                    .pointer("/capabilities/m.preview_url/enabled")
                    .is_some_and(|v| !v.is_boolean())
            {
                return Err(PreviewFailure::Unavailable);
            }
            if value.pointer("/capabilities/m.preview_url/enabled") == Some(&Value::Bool(false)) {
                return Err(PreviewFailure::Disabled);
            }
        }
        Err(PreviewFailure::Unsupported) => (),
        Err(e) => return Err(e),
    }
    let mut target = endpoint(hs, &["_matrix", "client", "v1", "media", "preview_url"])?;
    target.query_pairs_mut().append_pair("url", url);
    let bytes = request(http, target, token, RESPONSE_CAP).await?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| PreviewFailure::Unavailable)?;
    let metadata = preview_generation::normalize(url, &value).ok_or(PreviewFailure::Unavailable)?;
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
                if let Ok(bytes) = request(http, target, token, MAX_IMAGE_BYTES as usize).await {
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

    #[derive(Clone)]
    struct Fixture {
        mode: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
    }
    async fn capabilities(State(f): State<Fixture>, headers: HeaderMap) -> Response {
        assert_eq!(headers[header::AUTHORIZATION], "Bearer matrix-test-token");
        f.calls.lock().unwrap().push("capabilities".into());
        if f.mode == "disabled" {
            Json(serde_json::json!({"capabilities":{"m.preview_url":{"enabled":false}}}))
                .into_response()
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
            "fail" => StatusCode::BAD_GATEWAY.into_response(),
            "redirect" => (StatusCode::FOUND, [(header::LOCATION, "/must-not-follow")]).into_response(),
            "timeout" => { tokio::time::sleep(Duration::from_secs(1)).await; Json(serde_json::json!({})).into_response() },
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
    async fn preview_generation_boundaries_and_capability_policy() {
        for (mode, expected) in [
            ("disabled", PreviewFailure::Disabled),
            ("unsupported", PreviewFailure::Unsupported),
            ("forbidden", PreviewFailure::Disabled),
            ("fail", PreviewFailure::Unavailable),
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
                matches!(acquire_from(&http, &hs, "matrix-test-token", "https://example.org/private?value=1").await, Err(e) if e == expected),
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
            let result = acquire_from(
                &http,
                &hs,
                "matrix-test-token",
                "https://example.org/private?value=1",
            )
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
