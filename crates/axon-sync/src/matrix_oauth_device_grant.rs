//! OAuth 2.0 Device Authorization Grant requests for browser sign-in (ADR 0117).
//!
//! matrix-rust-sdk keeps its own device-grant requests private to QR login, so
//! the two requests live here beside discovery and dynamic registration and
//! follow the same rules: bounded timeouts, no redirects, size-capped bodies,
//! and stable log classifications that never include a response body, the user
//! code, the device code, or a token.

use std::time::Duration;

use matrix_sdk::ruma::{
    api::client::discovery::get_authorization_server_metadata::v1::{
        AuthorizationServerMetadata, GrantType,
    },
    DeviceId,
};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::matrix_oauth::{read_bounded_body, BoundedBodyError};

/// Scope granting full client-server API access.
///
/// These two strings must stay identical to the private `login_scopes` builder
/// in the pinned matrix-rust-sdk (`authentication/oauth/mod.rs`), so QR login
/// and browser sign-in present the same request to the authorization server.
/// Re-read them whenever the SDK pin moves (ADR 0117).
const SCOPE_CLIENT_API: &str = "urn:matrix:org.matrix.msc2967.client:api:*";
/// Prefix of the scope binding the token to one Matrix device ID.
const SCOPE_DEVICE_PREFIX: &str = "urn:matrix:org.matrix.msc2967.client:device:";
const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

const MAX_DEVICE_AUTHORIZATION_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_VERIFICATION_URI_BYTES: usize = 2_048;
const MAX_USER_CODE_BYTES: usize = 64;
const MAX_DEVICE_CODE_BYTES: usize = 4_096;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Consecutive transient token-endpoint failures tolerated before giving up.
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 5;

/// The OAuth error codes worth naming in a log (RFC 6749 §5.2, RFC 8628 §3.5).
const KNOWN_OAUTH_ERRORS: [&str; 12] = [
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
    "access_denied",
    "expired_token",
    "authorization_pending",
    "slow_down",
    "server_error",
    "temporarily_unavailable",
];

/// The `error` code of an OAuth error body, for the log.
///
/// The code is what tells an operator *why* a server refused: `invalid_scope`
/// means the scope names have drifted from what it accepts, `unauthorized_client`
/// that the registration may not use this grant. It is still text the remote
/// server chose, so only a code from the fixed list is ever returned. Anything
/// else is `other`, and a body with no readable code is `none`.
fn known_oauth_error(body: &[u8]) -> &'static str {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "none";
    };
    match json.get("error").and_then(serde_json::Value::as_str) {
        None => "none",
        Some(code) => KNOWN_OAUTH_ERRORS
            .iter()
            .find(|known| **known == code)
            .copied()
            .unwrap_or("other"),
    }
}

/// Why the grant ended without tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceGrantFailure {
    /// The authorization server does not offer the device grant.
    Unsupported,
    /// The flow was cancelled while waiting.
    Cancelled,
    /// The device authorization request did not answer in time.
    Timeout,
    /// The user refused at the authorization server.
    Denied,
    /// The device code or the flow deadline lapsed before approval.
    Expired,
    /// Anything else the authorization server did wrong.
    Upstream,
}

/// The validated endpoints one grant talks to.
#[derive(Debug)]
pub(crate) struct DeviceGrantEndpoints {
    device_authorization: Url,
    token: Url,
    pub(crate) revocation: Url,
    /// Whether a plain-HTTP loopback verification link is acceptable, which is
    /// true only when the homeserver itself is plain-HTTP loopback.
    allow_loopback_http: bool,
}

impl DeviceGrantEndpoints {
    /// Apply the capability gate: the endpoint must be advertised and the
    /// device grant must be among the grant types the server lists.
    pub(crate) fn from_metadata(
        homeserver: &Url,
        metadata: &AuthorizationServerMetadata,
    ) -> Result<Self, DeviceGrantFailure> {
        let Some(device_authorization) = metadata.device_authorization_endpoint.clone() else {
            tracing::warn!(
                phase = "device_authorization",
                error_class = "unsupported",
                reason = "no_endpoint",
                "authorization server does not offer the device authorization grant"
            );
            return Err(DeviceGrantFailure::Unsupported);
        };
        // Matrix requires the metadata to list its grant types, so an endpoint
        // advertised without this one is a server that does not offer it.
        if !metadata
            .grant_types_supported
            .contains(&GrantType::DeviceCode)
        {
            tracing::warn!(
                phase = "device_authorization",
                error_class = "unsupported",
                reason = "grant_type_not_listed",
                "authorization server does not offer the device authorization grant"
            );
            return Err(DeviceGrantFailure::Unsupported);
        }
        Ok(Self {
            device_authorization,
            token: metadata.token_endpoint.clone(),
            revocation: metadata.revocation_endpoint.clone(),
            allow_loopback_http: homeserver.scheme() == "http" && is_loopback(homeserver),
        })
    }
}

/// A validated device authorization response.
///
/// Deliberately not `Debug`: every field except the timings is a secret.
pub(crate) struct DeviceAuthorization {
    pub(crate) device_code: String,
    pub(crate) user_code: String,
    /// The link to present: `verification_uri_complete` when the server
    /// supplied one, the bare `verification_uri` otherwise.
    pub(crate) verification_uri: String,
    /// Whether that link already carries the user code, so the user need not
    /// type it. Logged, because "I was asked for a code" is a support question.
    pub(crate) has_complete_link: bool,
    pub(crate) interval: Duration,
    /// How long the device code lives, when the server said.
    pub(crate) expires_in: Option<Duration>,
}

/// Tokens from a successful poll. Deliberately not `Debug`.
pub(crate) struct DeviceGrantTokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: Option<String>,
}

/// One token-endpoint answer, reduced to what the poll loop acts on.
pub(crate) enum PollOutcome {
    Tokens(DeviceGrantTokens),
    Pending,
    SlowDown,
    Denied,
    Expired,
    /// Transport error, timeout, `429`, or `5xx`: retry at the same interval.
    Transient,
    /// Any other answer: the flow fails as `upstream`.
    Fatal,
}

/// The scope string for one device, in the pinned SDK's order.
pub(crate) fn login_scope(device_id: &DeviceId) -> String {
    format!("{SCOPE_CLIENT_API} {SCOPE_DEVICE_PREFIX}{device_id}")
}

/// Ask the authorization server for a device code, user code, and link.
pub(crate) async fn request_device_authorization(
    http: &reqwest::Client,
    endpoints: &DeviceGrantEndpoints,
    client_id: &str,
    device_id: &DeviceId,
) -> Result<DeviceAuthorization, DeviceGrantFailure> {
    const PHASE: &str = "device_authorization";
    let scope = login_scope(device_id);
    let body = form_body(&[("client_id", client_id), ("scope", &scope)]);
    let response = match post_form(http, &endpoints.device_authorization, body).await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => {
            tracing::warn!(
                phase = PHASE,
                error_class = "timeout",
                "Matrix OAuth request timed out"
            );
            return Err(DeviceGrantFailure::Timeout);
        }
        Err(_) => {
            tracing::warn!(
                phase = PHASE,
                error_class = "network",
                "Matrix OAuth request failed"
            );
            return Err(DeviceGrantFailure::Upstream);
        }
    };
    let status = response.status();
    if !status.is_success() {
        let body = read_bounded_body(response, MAX_DEVICE_AUTHORIZATION_RESPONSE_BYTES)
            .await
            .unwrap_or_default();
        tracing::warn!(
            phase = PHASE,
            error_class = "upstream",
            http_status = status.as_u16(),
            oauth_error = known_oauth_error(&body),
            "Matrix OAuth device authorization was refused"
        );
        return Err(DeviceGrantFailure::Upstream);
    }
    let invalid = |reason: &'static str| {
        tracing::warn!(
            phase = PHASE,
            error_class = "invalid_response",
            reason,
            http_status = status.as_u16(),
            "Matrix OAuth device authorization response was invalid"
        );
        DeviceGrantFailure::Upstream
    };
    let body = read_bounded_body(response, MAX_DEVICE_AUTHORIZATION_RESPONSE_BYTES)
        .await
        .map_err(|error| invalid(body_error_reason(&error)))?;
    parse_device_authorization(&body, endpoints.allow_loopback_http).map_err(invalid)
}

/// Parse and validate a device authorization response body. The error is a
/// stable reason for the log; it never contains any part of the response.
fn parse_device_authorization(
    body: &[u8],
    allow_loopback_http: bool,
) -> Result<DeviceAuthorization, &'static str> {
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|_| "not_json")?;
    let text = |field: &str| json.get(field).and_then(serde_json::Value::as_str);
    let seconds = |field: &str| match json.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or("invalid_duration"),
    };

    let device_code = text("device_code").ok_or("missing_device_code")?;
    if device_code.is_empty() || device_code.len() > MAX_DEVICE_CODE_BYTES {
        return Err("invalid_device_code");
    }
    let user_code = text("user_code").ok_or("missing_user_code")?;
    if !is_presentable_user_code(user_code) {
        return Err("invalid_user_code");
    }
    // Both links are validated even though only one is published: a server
    // that returns one unsafe link is not trusted for the other.
    let verification_uri = validate_verification_uri(
        text("verification_uri").ok_or("missing_verification_uri")?,
        allow_loopback_http,
    )
    .ok_or("unsafe_verification_uri")?;
    let verification_uri_complete = match json.get("verification_uri_complete") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|value| validate_verification_uri(value, allow_loopback_http))
                .ok_or("unsafe_verification_uri")?,
        ),
    };

    Ok(DeviceAuthorization {
        device_code: device_code.to_owned(),
        user_code: user_code.to_owned(),
        has_complete_link: verification_uri_complete.is_some(),
        verification_uri: verification_uri_complete
            .unwrap_or(verification_uri)
            .to_string(),
        interval: poll_interval(seconds("interval")?),
        expires_in: seconds("expires_in")?.map(Duration::from_secs),
    })
}

/// A short user code a client can render verbatim: printable ASCII only.
fn is_presentable_user_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_USER_CODE_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
}

/// A link a client can safely offer to open: an absolute `https` URL with a
/// host and no userinfo, within a fixed length. Plain `http` is accepted only
/// to a loopback host, and only when the homeserver is itself plain-HTTP
/// loopback. The link is not required to share an origin with the issuer.
fn validate_verification_uri(value: &str, allow_loopback_http: bool) -> Option<Url> {
    if value.is_empty() || value.len() > MAX_VERIFICATION_URI_BYTES {
        return None;
    }
    let url = Url::parse(value).ok()?;
    if url.host_str().is_none_or(str::is_empty) {
        return None;
    }
    let safe_scheme = url.scheme() == "https"
        || (url.scheme() == "http" && allow_loopback_http && is_loopback(&url));
    if !safe_scheme || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    Some(url)
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

/// The server's `interval`, five seconds when absent, and never less than one
/// second so a zero cannot make Axon hammer the token endpoint.
///
/// There is deliberately no upper bound. RFC 8628 §3.5 makes `interval` the
/// *minimum* wait, so polling sooner than asked invites throttling or a failed
/// authorization. A server that asks for an absurd wait only shortens its own
/// flow: the deadline ends it as `authorization_expired`.
fn poll_interval(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL)
        .max(MIN_POLL_INTERVAL)
}

/// Poll the token endpoint once with the device code.
pub(crate) async fn request_tokens(
    http: &reqwest::Client,
    endpoints: &DeviceGrantEndpoints,
    client_id: &str,
    device_code: &str,
) -> PollOutcome {
    const PHASE: &str = "device_token";
    let body = form_body(&[
        ("grant_type", DEVICE_CODE_GRANT_TYPE),
        ("device_code", device_code),
        ("client_id", client_id),
    ]);
    let response = match post_form(http, &endpoints.token, body).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                phase = PHASE,
                error_class = if error.is_timeout() {
                    "timeout"
                } else {
                    "network"
                },
                "Matrix OAuth token poll did not complete; will retry"
            );
            return PollOutcome::Transient;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        tracing::warn!(
            phase = PHASE,
            error_class = "upstream",
            http_status = status.as_u16(),
            "Matrix OAuth token endpoint is unavailable; will retry"
        );
        return PollOutcome::Transient;
    }
    let body = match read_bounded_body(response, MAX_TOKEN_RESPONSE_BYTES).await {
        Ok(body) => body,
        Err(BoundedBodyError::Transport(_)) => {
            tracing::warn!(
                phase = PHASE,
                error_class = "network",
                http_status = status.as_u16(),
                "Matrix OAuth token poll did not complete; will retry"
            );
            return PollOutcome::Transient;
        }
        Err(BoundedBodyError::TooLarge) => {
            tracing::warn!(
                phase = PHASE,
                error_class = "invalid_response",
                reason = "too_large",
                http_status = status.as_u16(),
                "Matrix OAuth token response was invalid"
            );
            return PollOutcome::Fatal;
        }
    };
    let outcome = classify_token_response(status.is_success(), &body);
    if matches!(outcome, PollOutcome::Fatal) {
        tracing::warn!(
            phase = PHASE,
            error_class = "upstream",
            http_status = status.as_u16(),
            // A success status with an unusable body is not an OAuth error.
            oauth_error = if status.is_success() {
                "none"
            } else {
                known_oauth_error(&body)
            },
            "Matrix OAuth token endpoint refused the device code"
        );
    }
    outcome
}

/// Map one fully-read token-endpoint answer onto the polling table. `429` and
/// `5xx` never reach here; they are transient whatever the body says.
fn classify_token_response(success: bool, body: &[u8]) -> PollOutcome {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(body) else {
        return PollOutcome::Fatal;
    };
    let text = |field: &str| json.get(field).and_then(serde_json::Value::as_str);
    if !success {
        return match text("error") {
            Some("authorization_pending") => PollOutcome::Pending,
            Some("slow_down") => PollOutcome::SlowDown,
            Some("access_denied") => PollOutcome::Denied,
            Some("expired_token") => PollOutcome::Expired,
            _ => PollOutcome::Fatal,
        };
    }
    let bearer = text("token_type").is_none_or(|kind| kind.eq_ignore_ascii_case("bearer"));
    match text("access_token") {
        Some(access_token) if bearer && !access_token.is_empty() => {
            PollOutcome::Tokens(DeviceGrantTokens {
                access_token: access_token.to_owned(),
                refresh_token: text("refresh_token")
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned),
            })
        }
        _ => PollOutcome::Fatal,
    }
}

/// Poll until the user approves, refuses, or runs out of time (RFC 8628 §3.5).
///
/// The first poll waits one interval. Every wait races cancellation and the
/// deadline; an in-flight poll is never abandoned, because a response dropped
/// on the floor could be the one carrying tokens that then nobody revokes.
pub(crate) async fn poll_for_tokens<F, Fut>(
    mut poll: F,
    mut interval: Duration,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
) -> Result<DeviceGrantTokens, DeviceGrantFailure>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = PollOutcome>,
{
    // How many polls were sent and how long the user took are what separate
    // "never approved" from "approved, then something else failed", so every
    // way out of this loop reports them.
    let started = tokio::time::Instant::now();
    let mut polls: u32 = 0;
    let mut consecutive_failures = 0;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(DeviceGrantFailure::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                tracing::warn!(
                    phase = "device_token",
                    error_class = "authorization_expired",
                    polls,
                    waited_secs = started.elapsed().as_secs(),
                    "Matrix OAuth device authorization was not approved in time"
                );
                return Err(DeviceGrantFailure::Expired);
            }
            _ = tokio::time::sleep(interval) => {}
        }
        polls = polls.saturating_add(1);
        match poll().await {
            PollOutcome::Tokens(tokens) => {
                tracing::info!(
                    phase = "device_token",
                    polls,
                    waited_secs = started.elapsed().as_secs(),
                    "Matrix OAuth device authorization approved"
                );
                return Ok(tokens);
            }
            PollOutcome::Pending => consecutive_failures = 0,
            PollOutcome::SlowDown => {
                consecutive_failures = 0;
                // Every `slow_down` adds five seconds, with no ceiling
                // (RFC 8628 §3.5); the deadline bounds the total.
                interval = interval.saturating_add(SLOW_DOWN_STEP);
                tracing::debug!(
                    phase = "device_token",
                    polls,
                    interval_secs = interval.as_secs(),
                    "Matrix OAuth token endpoint asked for slower polling"
                );
            }
            PollOutcome::Denied => {
                tracing::warn!(
                    phase = "device_token",
                    error_class = "authorization_denied",
                    polls,
                    waited_secs = started.elapsed().as_secs(),
                    "Matrix OAuth device authorization was refused by the user"
                );
                return Err(DeviceGrantFailure::Denied);
            }
            PollOutcome::Expired => {
                tracing::warn!(
                    phase = "device_token",
                    error_class = "authorization_expired",
                    polls,
                    waited_secs = started.elapsed().as_secs(),
                    "Matrix OAuth device code expired before approval"
                );
                return Err(DeviceGrantFailure::Expired);
            }
            PollOutcome::Transient => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                    tracing::warn!(
                        phase = "device_token",
                        error_class = "upstream",
                        consecutive_failures,
                        polls,
                        "Matrix OAuth token endpoint kept failing; giving up"
                    );
                    return Err(DeviceGrantFailure::Upstream);
                }
            }
            PollOutcome::Fatal => return Err(DeviceGrantFailure::Upstream),
        }
    }
}

/// Best-effort revocation of a token that never reached an SDK session, so
/// `client.oauth().logout()` cannot revoke it. Never fails the caller.
pub(crate) async fn revoke_token(
    http: &reqwest::Client,
    endpoints: &DeviceGrantEndpoints,
    client_id: &str,
    token: &str,
) {
    let body = form_body(&[("token", token), ("client_id", client_id)]);
    match post_form(http, &endpoints.revocation, body).await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => tracing::warn!(
            phase = "device_token_revocation",
            error_class = "upstream",
            http_status = response.status().as_u16(),
            "Matrix OAuth token revocation failed"
        ),
        Err(error) => tracing::warn!(
            phase = "device_token_revocation",
            error_class = if error.is_timeout() {
                "timeout"
            } else {
                "network"
            },
            "Matrix OAuth token revocation failed"
        ),
    }
}

fn form_body(fields: &[(&str, &str)]) -> String {
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        body.append_pair(name, value);
    }
    body.finish()
}

async fn post_form(
    http: &reqwest::Client,
    endpoint: &Url,
    body: String,
) -> Result<reqwest::Response, reqwest::Error> {
    http.post(endpoint.clone())
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .body(body)
        .send()
        .await
}

fn body_error_reason(error: &BoundedBodyError) -> &'static str {
    match error {
        BoundedBodyError::TooLarge => "too_large",
        BoundedBodyError::Transport(_) => "truncated",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use axum::{
        extract::State,
        http::{header, StatusCode},
        response::IntoResponse,
        routing::post,
        Router,
    };
    use matrix_sdk::ruma::{device_id, serde::Raw};

    use super::*;

    fn metadata(json: serde_json::Value) -> AuthorizationServerMetadata {
        let mut base = serde_json::json!({
            "issuer": "https://auth.example.org/",
            "authorization_endpoint": "https://auth.example.org/authorize",
            "token_endpoint": "https://auth.example.org/token",
            "response_types_supported": ["code"],
            "response_modes_supported": ["query", "fragment"],
            "grant_types_supported": [
                "authorization_code",
                "refresh_token",
                "urn:ietf:params:oauth:grant-type:device_code"
            ],
            "revocation_endpoint": "https://auth.example.org/revoke",
            "code_challenge_methods_supported": ["S256"],
            "device_authorization_endpoint": "https://auth.example.org/device"
        });
        for (key, value) in json.as_object().unwrap() {
            if value.is_null() {
                base.as_object_mut().unwrap().remove(key);
            } else {
                base[key] = value.clone();
            }
        }
        serde_json::from_value::<Raw<AuthorizationServerMetadata>>(base)
            .unwrap()
            .deserialize()
            .unwrap()
    }

    fn https_homeserver() -> Url {
        Url::parse("https://matrix.example.org/").unwrap()
    }

    fn authorization(json: serde_json::Value) -> Result<DeviceAuthorization, &'static str> {
        parse_device_authorization(json.to_string().as_bytes(), false)
    }

    fn valid_authorization() -> serde_json::Value {
        serde_json::json!({
            "device_code": "device-secret",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://auth.example.org/link",
            "verification_uri_complete": "https://auth.example.org/link?code=WDJB-MJHT",
            "expires_in": 1200,
            "interval": 5
        })
    }

    #[test]
    fn scope_names_the_api_and_exactly_the_given_device() {
        assert_eq!(
            login_scope(device_id!("AXONDEVICE")),
            "urn:matrix:org.matrix.msc2967.client:api:* \
             urn:matrix:org.matrix.msc2967.client:device:AXONDEVICE"
        );
    }

    #[test]
    fn capability_gate_needs_the_endpoint_and_a_listed_grant_type() {
        let offered = DeviceGrantEndpoints::from_metadata(
            &https_homeserver(),
            &metadata(serde_json::json!({})),
        )
        .expect("advertised endpoint and grant type");
        assert_eq!(
            offered.device_authorization.as_str(),
            "https://auth.example.org/device"
        );
        assert_eq!(offered.token.as_str(), "https://auth.example.org/token");
        assert!(!offered.allow_loopback_http);

        let no_endpoint = metadata(serde_json::json!({ "device_authorization_endpoint": null }));
        assert!(matches!(
            DeviceGrantEndpoints::from_metadata(&https_homeserver(), &no_endpoint),
            Err(DeviceGrantFailure::Unsupported)
        ));

        // An endpoint advertised without the grant type is not an offer.
        let no_grant = metadata(serde_json::json!({
            "grant_types_supported": ["authorization_code", "refresh_token"]
        }));
        assert!(matches!(
            DeviceGrantEndpoints::from_metadata(&https_homeserver(), &no_grant),
            Err(DeviceGrantFailure::Unsupported)
        ));
    }

    #[test]
    fn loopback_http_links_need_a_plain_http_loopback_homeserver() {
        let offered = |homeserver: &str| {
            DeviceGrantEndpoints::from_metadata(
                &Url::parse(homeserver).unwrap(),
                &metadata(serde_json::json!({})),
            )
            .ok()
            .unwrap()
            .allow_loopback_http
        };
        assert!(offered("http://127.0.0.1:8008/"));
        assert!(offered("http://localhost:8008/"));
        assert!(offered("http://[::1]:8008/"));
        assert!(!offered("https://localhost:8448/"));
        assert!(!offered("http://matrix.example.org/"));
    }

    #[test]
    fn a_valid_response_publishes_the_complete_link_when_there_is_one() {
        let parsed = authorization(valid_authorization()).ok().unwrap();
        assert_eq!(parsed.device_code, "device-secret");
        assert_eq!(parsed.user_code, "WDJB-MJHT");
        assert_eq!(
            parsed.verification_uri,
            "https://auth.example.org/link?code=WDJB-MJHT"
        );
        assert_eq!(parsed.interval, Duration::from_secs(5));
        assert_eq!(parsed.expires_in, Some(Duration::from_secs(1200)));
        assert!(parsed.has_complete_link);

        let mut bare = valid_authorization();
        bare.as_object_mut()
            .unwrap()
            .remove("verification_uri_complete");
        assert_eq!(
            authorization(bare).ok().unwrap().verification_uri,
            "https://auth.example.org/link"
        );
    }

    #[test]
    fn an_unsafe_link_in_either_field_rejects_the_whole_response() {
        let unsafe_links = [
            "javascript:alert(1)".to_owned(),
            "data:text/html,<script>alert(1)</script>".to_owned(),
            "http://auth.example.org/link".to_owned(),
            "http://127.0.0.1:8080/link".to_owned(),
            "https://user:password@auth.example.org/link".to_owned(),
            "https://user@auth.example.org/link".to_owned(),
            "/link".to_owned(),
            String::new(),
            format!(
                "https://auth.example.org/{}",
                "a".repeat(MAX_VERIFICATION_URI_BYTES)
            ),
        ];
        for link in unsafe_links {
            for field in ["verification_uri", "verification_uri_complete"] {
                let mut response = valid_authorization();
                response[field] = serde_json::Value::String(link.clone());
                assert!(
                    authorization(response).is_err(),
                    "{field} accepted an unsafe link"
                );
            }
        }
        let mut not_a_string = valid_authorization();
        not_a_string["verification_uri_complete"] = serde_json::json!(42);
        assert!(authorization(not_a_string).is_err());
    }

    #[test]
    fn loopback_http_link_is_accepted_only_when_allowed() {
        let mut response = valid_authorization();
        response["verification_uri"] = serde_json::json!("http://localhost:8080/link");
        response["verification_uri_complete"] = serde_json::json!("http://[::1]:8080/link?code=A");
        let body = response.to_string();
        assert!(parse_device_authorization(body.as_bytes(), false).is_err());
        let parsed = parse_device_authorization(body.as_bytes(), true)
            .ok()
            .unwrap();
        assert_eq!(parsed.verification_uri, "http://[::1]:8080/link?code=A");

        // The exemption is for loopback hosts only, never any plain-HTTP host.
        response["verification_uri_complete"] = serde_json::json!("http://auth.example.org/link");
        assert!(parse_device_authorization(response.to_string().as_bytes(), true).is_err());
    }

    #[test]
    fn an_unpresentable_user_code_rejects_the_response() {
        for code in [
            String::new(),
            "A".repeat(MAX_USER_CODE_BYTES + 1),
            "ABCD\nEFGH".to_owned(),
            "ABCD\u{1b}[31m".to_owned(),
            "ＡＢＣＤ".to_owned(),
            "ABCD\u{202e}EFGH".to_owned(),
        ] {
            let mut response = valid_authorization();
            response["user_code"] = serde_json::Value::String(code);
            assert!(authorization(response).is_err());
        }
        for code in ["WDJB-MJHT", "WDJB MJHT", &"A".repeat(MAX_USER_CODE_BYTES)] {
            let mut response = valid_authorization();
            response["user_code"] = serde_json::json!(code);
            assert!(authorization(response).is_ok());
        }
    }

    #[test]
    fn missing_or_malformed_fields_reject_the_response() {
        for field in ["device_code", "user_code", "verification_uri"] {
            let mut response = valid_authorization();
            response.as_object_mut().unwrap().remove(field);
            assert!(authorization(response).is_err(), "{field} is required");
        }
        let mut oversized = valid_authorization();
        oversized["device_code"] = serde_json::json!("d".repeat(MAX_DEVICE_CODE_BYTES + 1));
        assert!(authorization(oversized).is_err());
        let mut negative = valid_authorization();
        negative["interval"] = serde_json::json!(-1);
        assert!(authorization(negative).is_err());
        assert!(parse_device_authorization(b"<html>not json</html>", false).is_err());
    }

    #[test]
    fn interval_defaults_to_five_seconds_and_is_never_shortened() {
        assert_eq!(poll_interval(None), Duration::from_secs(5));
        // Zero would hammer the endpoint, so it alone is raised.
        assert_eq!(poll_interval(Some(0)), Duration::from_secs(1));
        assert_eq!(poll_interval(Some(1)), Duration::from_secs(1));
        assert_eq!(poll_interval(Some(30)), Duration::from_secs(30));
        // `interval` is the server's minimum wait (RFC 8628 §3.5): a long one
        // is honoured, never cut down to something Axon finds convenient.
        assert_eq!(poll_interval(Some(120)), Duration::from_secs(120));
        assert_eq!(poll_interval(Some(86_400)), Duration::from_secs(86_400));
        assert_eq!(poll_interval(Some(u64::MAX)), Duration::from_secs(u64::MAX));

        let mut absent = valid_authorization();
        absent.as_object_mut().unwrap().remove("interval");
        absent.as_object_mut().unwrap().remove("expires_in");
        let parsed = authorization(absent).ok().unwrap();
        assert_eq!(parsed.interval, Duration::from_secs(5));
        assert_eq!(parsed.expires_in, None);
    }

    #[test]
    fn only_standard_oauth_error_codes_reach_the_log() {
        let code = |body: &str| known_oauth_error(body.as_bytes());
        for known in KNOWN_OAUTH_ERRORS {
            assert_eq!(code(&format!(r#"{{"error":"{known}"}}"#)), known);
        }
        // Text the server made up is never echoed, however harmless it looks.
        assert_eq!(code(r#"{"error":"made_up_code"}"#), "other");
        assert_eq!(
            code(r#"{"error":"invalid_scope\nERROR forged log line"}"#),
            "other"
        );
        assert_eq!(code(r#"{"error":42}"#), "none");
        assert_eq!(code(r#"{"error_description":"secret detail"}"#), "none");
        assert_eq!(code("<html>Bad Request</html>"), "none");
        assert_eq!(code(""), "none");
    }

    fn classify(success: bool, json: serde_json::Value) -> PollOutcome {
        classify_token_response(success, json.to_string().as_bytes())
    }

    #[test]
    fn token_responses_follow_the_polling_table() {
        let error = |code: &str| classify(false, serde_json::json!({ "error": code }));
        assert!(matches!(
            error("authorization_pending"),
            PollOutcome::Pending
        ));
        assert!(matches!(error("slow_down"), PollOutcome::SlowDown));
        assert!(matches!(error("access_denied"), PollOutcome::Denied));
        assert!(matches!(error("expired_token"), PollOutcome::Expired));
        for other in ["invalid_grant", "server_error", "invalid_client", "made_up"] {
            assert!(matches!(error(other), PollOutcome::Fatal));
        }
        assert!(matches!(
            classify(false, serde_json::json!({})),
            PollOutcome::Fatal
        ));
        assert!(matches!(
            classify_token_response(false, b"<html>Bad Request</html>"),
            PollOutcome::Fatal
        ));
        // A pending error on a successful status is not a token response.
        assert!(matches!(
            classify(
                true,
                serde_json::json!({ "error": "authorization_pending" })
            ),
            PollOutcome::Fatal
        ));
    }

    #[test]
    fn a_token_response_keeps_its_refresh_token_only_when_present() {
        let PollOutcome::Tokens(tokens) = classify(
            true,
            serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "token_type": "Bearer",
                "expires_in": 300
            }),
        ) else {
            panic!("expected tokens");
        };
        assert_eq!(tokens.access_token, "access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh"));

        let PollOutcome::Tokens(tokens) = classify(
            true,
            serde_json::json!({ "access_token": "access", "refresh_token": "" }),
        ) else {
            panic!("expected tokens");
        };
        assert!(tokens.refresh_token.is_none());

        for invalid in [
            serde_json::json!({ "refresh_token": "refresh" }),
            serde_json::json!({ "access_token": "" }),
            serde_json::json!({ "access_token": "access", "token_type": "mac" }),
        ] {
            assert!(matches!(classify(true, invalid), PollOutcome::Fatal));
        }
        assert!(matches!(
            classify_token_response(true, b"not json"),
            PollOutcome::Fatal
        ));
    }

    /// Scripted token endpoint for the poll loop, recording when it was asked.
    struct Script {
        outcomes: Mutex<VecDeque<PollOutcome>>,
        polled_at: Mutex<Vec<Duration>>,
        started: tokio::time::Instant,
    }

    impl Script {
        fn new(outcomes: Vec<PollOutcome>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: Mutex::new(outcomes.into()),
                polled_at: Mutex::new(Vec::new()),
                started: tokio::time::Instant::now(),
            })
        }

        fn poll(self: &Arc<Self>) -> impl std::future::Future<Output = PollOutcome> {
            let script = self.clone();
            async move {
                script
                    .polled_at
                    .lock()
                    .unwrap()
                    .push(script.started.elapsed());
                script
                    .outcomes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(PollOutcome::Pending)
            }
        }

        fn polled_at_secs(&self) -> Vec<u64> {
            self.polled_at
                .lock()
                .unwrap()
                .iter()
                .map(Duration::as_secs)
                .collect()
        }
    }

    fn tokens() -> PollOutcome {
        PollOutcome::Tokens(DeviceGrantTokens {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
        })
    }

    fn far_deadline() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(24 * 60 * 60)
    }

    #[tokio::test(start_paused = true)]
    async fn first_poll_waits_one_interval_and_pending_keeps_the_pace() {
        let script = Script::new(vec![PollOutcome::Pending, PollOutcome::Pending, tokens()]);
        let result = poll_for_tokens(
            || script.poll(),
            Duration::from_secs(5),
            far_deadline(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.ok().unwrap().access_token, "access");
        assert_eq!(script.polled_at_secs(), [5, 10, 15]);
    }

    #[tokio::test(start_paused = true)]
    async fn every_slow_down_adds_five_seconds_without_a_ceiling() {
        let mut outcomes: Vec<PollOutcome> = (0..13).map(|_| PollOutcome::SlowDown).collect();
        outcomes.push(tokens());
        let script = Script::new(outcomes);
        poll_for_tokens(
            || script.poll(),
            Duration::from_secs(5),
            far_deadline(),
            &CancellationToken::new(),
        )
        .await
        .ok()
        .unwrap();
        let polled = script.polled_at_secs();
        let gaps: Vec<u64> = polled.windows(2).map(|pair| pair[1] - pair[0]).collect();
        assert_eq!(gaps, [10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70]);
    }

    /// RFC 8628 §3.5: the server's interval is a minimum, and `slow_down`
    /// raises it by five seconds however long it already is.
    #[tokio::test(start_paused = true)]
    async fn a_long_server_interval_is_honoured_and_still_slows_down() {
        let script = Script::new(vec![PollOutcome::SlowDown, tokens()]);
        poll_for_tokens(
            || script.poll(),
            Duration::from_secs(120),
            far_deadline(),
            &CancellationToken::new(),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(script.polled_at_secs(), [120, 245]);
    }

    /// A server cannot stall a flow by asking for a long wait: the deadline
    /// ends it, without a poll sooner than the server allowed.
    #[tokio::test(start_paused = true)]
    async fn the_deadline_bounds_a_wait_the_server_made_too_long() {
        for interval in [Duration::from_secs(120), Duration::from_secs(u64::MAX)] {
            let script = Script::new(vec![tokens()]);
            let result = poll_for_tokens(
                || script.poll(),
                interval,
                tokio::time::Instant::now() + Duration::from_secs(100),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(result.err(), Some(DeviceGrantFailure::Expired));
            assert!(script.polled_at_secs().is_empty());
        }

        // Growth by `slow_down` cannot overflow the interval either.
        let script = Script::new(vec![PollOutcome::SlowDown]);
        let result = poll_for_tokens(
            || script.poll(),
            Duration::MAX - Duration::from_secs(1),
            tokio::time::Instant::now() + Duration::from_secs(100),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.err(), Some(DeviceGrantFailure::Expired));
    }

    #[tokio::test(start_paused = true)]
    async fn denial_and_expiry_end_the_flow_with_their_own_classes() {
        for (outcome, expected) in [
            (PollOutcome::Denied, DeviceGrantFailure::Denied),
            (PollOutcome::Expired, DeviceGrantFailure::Expired),
            (PollOutcome::Fatal, DeviceGrantFailure::Upstream),
        ] {
            let script = Script::new(vec![PollOutcome::Pending, outcome]);
            let result = poll_for_tokens(
                || script.poll(),
                Duration::from_secs(5),
                far_deadline(),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(result.err(), Some(expected));
            assert_eq!(script.polled_at_secs().len(), 2);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn five_consecutive_transient_failures_give_up() {
        let script = Script::new((0..5).map(|_| PollOutcome::Transient).collect());
        let result = poll_for_tokens(
            || script.poll(),
            Duration::from_secs(5),
            far_deadline(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.err(), Some(DeviceGrantFailure::Upstream));
        // Retries keep the current interval; they do not back off or speed up.
        assert_eq!(script.polled_at_secs(), [5, 10, 15, 20, 25]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_resets_the_transient_failure_budget() {
        let mut outcomes = Vec::new();
        for _ in 0..3 {
            outcomes.extend((0..4).map(|_| PollOutcome::Transient));
            outcomes.push(PollOutcome::Pending);
        }
        outcomes.push(tokens());
        let script = Script::new(outcomes);
        let result = poll_for_tokens(
            || script.poll(),
            Duration::from_secs(5),
            far_deadline(),
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(script.polled_at_secs().len(), 16);
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_expires_a_pending_authorization() {
        let script = Script::new(Vec::new());
        let result = poll_for_tokens(
            || script.poll(),
            Duration::from_secs(5),
            tokio::time::Instant::now() + Duration::from_secs(12),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result.err(), Some(DeviceGrantFailure::Expired));
        assert_eq!(script.polled_at_secs(), [5, 10]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_a_wait() {
        let script = Script::new(Vec::new());
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(7)).await;
            canceller.cancel();
        });
        let result = poll_for_tokens(
            || script.poll(),
            Duration::from_secs(5),
            far_deadline(),
            &cancel,
        )
        .await;
        assert_eq!(result.err(), Some(DeviceGrantFailure::Cancelled));
        assert_eq!(script.polled_at_secs(), [5]);
    }

    #[derive(Clone)]
    struct Recorded(Arc<Mutex<Vec<String>>>);

    async fn serve(app: Router) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, server)
    }

    fn http() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
    }

    fn endpoints(base: &Url) -> DeviceGrantEndpoints {
        DeviceGrantEndpoints {
            device_authorization: base.join("device").unwrap(),
            token: base.join("token").unwrap(),
            revocation: base.join("revoke").unwrap(),
            allow_loopback_http: true,
        }
    }

    #[tokio::test]
    async fn device_authorization_request_sends_the_client_and_both_scopes() {
        let recorded = Recorded(Arc::new(Mutex::new(Vec::new())));
        let app =
            Router::new()
                .route(
                    "/device",
                    post(
                        |State(recorded): State<Recorded>,
                         headers: header::HeaderMap,
                         body: String| async move {
                            assert_eq!(
                                headers[header::CONTENT_TYPE],
                                "application/x-www-form-urlencoded"
                            );
                            recorded.0.lock().unwrap().push(body);
                            axum::Json(serde_json::json!({
                                "device_code": "device-secret",
                                "user_code": "WDJB-MJHT",
                                "verification_uri": "http://127.0.0.1:1/link",
                                "expires_in": 600
                            }))
                        },
                    ),
                )
                .with_state(recorded.clone());
        let (base, server) = serve(app).await;

        let authorization = request_device_authorization(
            &http(),
            &endpoints(&base),
            "axon-client",
            device_id!("AXONDEVICE"),
        )
        .await
        .ok()
        .unwrap();
        assert_eq!(authorization.user_code, "WDJB-MJHT");
        assert_eq!(authorization.verification_uri, "http://127.0.0.1:1/link");

        let bodies = recorded.0.lock().unwrap().clone();
        let fields: Vec<(String, String)> = url::form_urlencoded::parse(bodies[0].as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            fields,
            [
                ("client_id".to_owned(), "axon-client".to_owned()),
                (
                    "scope".to_owned(),
                    "urn:matrix:org.matrix.msc2967.client:api:* \
                     urn:matrix:org.matrix.msc2967.client:device:AXONDEVICE"
                        .to_owned()
                ),
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn hostile_device_authorization_responses_fail_as_upstream() {
        let app = Router::new()
            .route(
                "/oversized/device",
                post(|| async { vec![b' '; MAX_DEVICE_AUTHORIZATION_RESPONSE_BYTES + 1] }),
            )
            .route("/html/device", post(|| async { "<html>Sign in</html>" }))
            .route(
                "/redirect/device",
                post(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "https://elsewhere.example.org/device")],
                    )
                }),
            )
            .route(
                "/refused/device",
                post(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({ "error": "unauthorized_client" })),
                    )
                }),
            );
        let (base, server) = serve(app).await;
        for prefix in ["oversized/", "html/", "redirect/", "refused/", "missing/"] {
            let result = request_device_authorization(
                &http(),
                &endpoints(&base.join(prefix).unwrap()),
                "axon-client",
                device_id!("AXONDEVICE"),
            )
            .await;
            assert_eq!(
                result.err(),
                Some(DeviceGrantFailure::Upstream),
                "{prefix} should fail as upstream"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn an_unanswered_device_authorization_request_is_a_timeout() {
        let app = Router::new().route(
            "/device",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                StatusCode::OK
            }),
        );
        let (base, server) = serve(app).await;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let result = request_device_authorization(
            &http,
            &endpoints(&base),
            "axon-client",
            device_id!("AXONDEVICE"),
        )
        .await;
        assert_eq!(result.err(), Some(DeviceGrantFailure::Timeout));
        server.abort();
    }

    #[tokio::test]
    async fn token_request_sends_the_grant_and_classifies_each_status() {
        let recorded = Recorded(Arc::new(Mutex::new(Vec::new())));
        let pending = || async {
            (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": "authorization_pending" })),
            )
        };
        let app = Router::new()
            .route(
                "/ok/token",
                post(
                    |State(recorded): State<Recorded>, body: String| async move {
                        recorded.0.lock().unwrap().push(body);
                        axum::Json(serde_json::json!({
                            "access_token": "access",
                            "refresh_token": "refresh",
                            "token_type": "Bearer"
                        }))
                    },
                ),
            )
            .route("/pending/token", post(pending))
            .route(
                "/busy/token",
                post(|| async {
                    // Transient whatever the body claims.
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        axum::Json(serde_json::json!({ "error": "access_denied" })),
                    )
                }),
            )
            .route(
                "/down/token",
                post(|| async { StatusCode::BAD_GATEWAY.into_response() }),
            )
            .route(
                "/redirect/token",
                post(|| async {
                    (
                        StatusCode::TEMPORARY_REDIRECT,
                        [(header::LOCATION, "https://elsewhere.example.org/token")],
                    )
                }),
            )
            .route(
                "/oversized/token",
                post(|| async { vec![b' '; MAX_TOKEN_RESPONSE_BYTES + 1] }),
            )
            .route("/html/token", post(|| async { "<html>Sign in</html>" }))
            .with_state(recorded.clone());
        let (base, server) = serve(app).await;
        let poll = |prefix: &'static str| {
            let base = base.clone();
            async move {
                request_tokens(
                    &http(),
                    &endpoints(&base.join(prefix).unwrap()),
                    "axon-client",
                    "device-secret",
                )
                .await
            }
        };

        assert!(matches!(poll("ok/").await, PollOutcome::Tokens(_)));
        assert!(matches!(poll("pending/").await, PollOutcome::Pending));
        assert!(matches!(poll("busy/").await, PollOutcome::Transient));
        assert!(matches!(poll("down/").await, PollOutcome::Transient));
        assert!(matches!(poll("redirect/").await, PollOutcome::Fatal));
        assert!(matches!(poll("oversized/").await, PollOutcome::Fatal));
        assert!(matches!(poll("html/").await, PollOutcome::Fatal));
        assert!(matches!(poll("missing/").await, PollOutcome::Fatal));

        let bodies = recorded.0.lock().unwrap().clone();
        let fields: Vec<(String, String)> = url::form_urlencoded::parse(bodies[0].as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            fields,
            [
                (
                    "grant_type".to_owned(),
                    "urn:ietf:params:oauth:grant-type:device_code".to_owned()
                ),
                ("device_code".to_owned(), "device-secret".to_owned()),
                ("client_id".to_owned(), "axon-client".to_owned()),
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn an_unreachable_token_endpoint_is_transient() {
        // Bound but never accepted from, then dropped: the port refuses.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        assert!(matches!(
            request_tokens(&http(), &endpoints(&base), "axon-client", "device-secret").await,
            PollOutcome::Transient
        ));
    }
}
