//! APNs and FCM sends (ADR 0113, ADR 0129).
//!
//! A failure records the HTTP status and, when the body parses, the APNs
//! `reason` or the FCM `error.status`. A success records the provider, the
//! registration, the account, the room, and the status at debug. A badge-only
//! push has no account or room, and records the total instead. None of them
//! records the URL, the device token, the authorization header, the alert, or
//! the raw body. `reqwest`'s `Display` includes the URL, so it is never logged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axon_core::{LoadedApns, LoadedFcm, PushMaterial};
use axon_store::{PushTarget, Store};
use chrono::{DateTime, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::message::{at_localpart, PushMessage};
use super::payload;

const LIVENESS_TIMEOUT: Duration = Duration::from_secs(2);
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);
const APNS_JWT_TTL: Duration = Duration::from_secs(50 * 60);
const FCM_REFRESH_SLACK: Duration = Duration::from_secs(60);
const BODY_CAP: usize = 64 * 1024;

/// What a send is about, for its log lines. The badge-only push is about the
/// device, not a room.
#[derive(Debug, Clone, Copy)]
enum Subject<'a> {
    Room { account_id: Uuid, room_id: &'a str },
    Badge { total: u64 },
}

/// The APNs headers that differ between an alert and a badge-only push.
///
/// Both use `apns-push-type: alert`, which Apple requires for a badge. The
/// alert is priority 10 and collapses per room. The badge-only push is
/// priority 5, so the device's power policy may batch it, and has no collapse
/// id (ADR 0129).
#[derive(Debug, Clone, Copy)]
struct ApnsHeaders<'a> {
    priority: &'static str,
    collapse: Option<&'a str>,
}

/// What a provider response means for the registration row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderVerdict {
    Sent,
    /// The device token is dead. Delete the registration.
    Delete,
    /// Keep the row. Do not retry inside this call.
    Keep,
}

/// A provider failure that is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderFailure {
    status: Option<u16>,
    reason: Option<String>,
    kind: &'static str,
}

impl ProviderFailure {
    fn status(status: u16, reason: Option<String>) -> Self {
        Self {
            status: Some(status),
            reason,
            kind: "status",
        }
    }

    fn transport(kind: &'static str) -> Self {
        Self {
            status: None,
            reason: None,
            kind,
        }
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.status, self.reason.as_deref()) {
            (Some(status), Some(reason)) => write!(f, "push provider status {status} ({reason})"),
            (Some(status), None) => write!(f, "push provider status {status}"),
            (None, _) => write!(f, "push provider {kind}", kind = self.kind),
        }
    }
}

impl std::error::Error for ProviderFailure {}

pub(crate) fn apns_verdict(status: u16, body: &[u8], updated_at: DateTime<Utc>) -> ProviderVerdict {
    if (200..300).contains(&status) {
        return ProviderVerdict::Sent;
    }
    let parsed = parse_reason(body);
    if status == 410 && parsed.reason.as_deref() == Some("Unregistered") {
        let Some(timestamp) = parsed.timestamp_ms else {
            return ProviderVerdict::Keep;
        };
        return if timestamp > updated_at.timestamp_millis() {
            ProviderVerdict::Delete
        } else {
            ProviderVerdict::Keep
        };
    }
    if status == 400 && parsed.reason.as_deref() == Some("BadDeviceToken") {
        return ProviderVerdict::Delete;
    }
    ProviderVerdict::Keep
}

pub(crate) fn fcm_verdict(status: u16, body: &[u8]) -> ProviderVerdict {
    if (200..300).contains(&status) {
        return ProviderVerdict::Sent;
    }
    if status == 404 && parse_reason(body).reason.as_deref() == Some("UNREGISTERED") {
        return ProviderVerdict::Delete;
    }
    ProviderVerdict::Keep
}

/// A 401 means FCM refused the cached OAuth bearer.
///
/// The registration stays. The next send mints a new access token instead of
/// reusing one that was revoked or signed by a key that has since rotated.
fn fcm_bearer_rejected(status: u16) -> bool {
    status == 401
}

struct ParsedReason {
    reason: Option<String>,
    timestamp_ms: Option<i64>,
}

fn parse_reason(body: &[u8]) -> ParsedReason {
    if body.len() > BODY_CAP {
        return ParsedReason {
            reason: None,
            timestamp_ms: None,
        };
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return ParsedReason {
            reason: None,
            timestamp_ms: None,
        };
    };
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            value
                .pointer("/error/status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    ParsedReason {
        reason,
        timestamp_ms: value.get("timestamp").and_then(serde_json::Value::as_i64),
    }
}

fn transport_kind(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else {
        "transport"
    }
}

/// Percent-encode a form value. The FCM assertion is a JWT and must not be logged.
fn form_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

struct CachedJwt {
    value: String,
    minted: Instant,
}

struct CachedFcm {
    value: String,
    good_until: Instant,
}

pub(crate) struct LiveTransport {
    store: Store,
    http: reqwest::Client,
    material: PushMaterial,
    apns_jwt: Mutex<Option<CachedJwt>>,
    fcm_token: Mutex<Option<CachedFcm>>,
    fcm_refresh: tokio::sync::Mutex<()>,
    apns_warned: AtomicBool,
    fcm_warned: AtomicBool,
}

impl LiveTransport {
    pub(crate) fn new(store: Store, material: PushMaterial) -> Self {
        Self {
            store,
            http: push_http_client(),
            material,
            apns_jwt: Mutex::new(None),
            fcm_token: Mutex::new(None),
            fcm_refresh: tokio::sync::Mutex::new(()),
            apns_warned: AtomicBool::new(false),
            fcm_warned: AtomicBool::new(false),
        }
    }

    /// Send one alert. The snapshot token is used only when the liveness read fails.
    ///
    /// `banner` is shared by every device receiving this message, so the name
    /// lookup runs once.
    pub(crate) async fn deliver(
        &self,
        snapshot: &PushTarget,
        account_id: Uuid,
        room_id: &str,
        message: &PushMessage,
        banner: &SharedBanner,
        cancel: &CancellationToken,
    ) {
        if cancel.is_cancelled() {
            return;
        }
        let target = match self.resolve_target(snapshot).await {
            Some(target) => target,
            None => return,
        };
        if cancel.is_cancelled() {
            return;
        }
        let (room_name, sender_display) = banner.resolve(&self.store).await;
        let badge = if target.provider == "apns" && target.badge {
            banner.badge_total(&self.store).await
        } else {
            None
        };
        let account = account_id.to_string();
        let Some((body, collapse)) = payload::fit_outbound(&payload::OutboundRequest {
            provider: &target.provider,
            disclosure: &target.disclosure,
            room_name: room_name.as_deref(),
            sender_display: &sender_display,
            account_id: &account,
            room_id,
            message,
            fcm_token: &target.token,
            badge,
        }) else {
            tracing::warn!(
                registration_id = %target.id,
                %account_id,
                room_id,
                event_id = %message.event_id,
                "push payload exceeded 4096 bytes; wake dropped"
            );
            return;
        };
        let subject = Subject::Room {
            account_id,
            room_id,
        };
        let verdict = match target.provider.as_str() {
            "apns" => {
                let headers = ApnsHeaders {
                    priority: "10",
                    collapse: Some(&collapse),
                };
                self.send_configured_apns(&target, subject, headers, body)
                    .await
            }
            "fcm" => self.send_configured_fcm(&target, subject, body).await,
            other => {
                tracing::warn!(
                    registration_id = %target.id,
                    provider = other,
                    "push registration has an unknown provider"
                );
                return;
            }
        };
        if verdict == ProviderVerdict::Delete {
            self.delete_registration(target.id, "provider rejected the device token")
                .await;
        }
    }

    /// Send the instance unread total as a badge-only APNs push (ADR 0129).
    ///
    /// The total is read here, after the liveness check, so a burst that
    /// coalesced into this one send carries the newest number. A registration
    /// that has since turned the badge off, or that is not APNs, sends nothing.
    /// A failed or slow total read skips the push: a guessed number is worse
    /// than the one already on the icon.
    pub(crate) async fn deliver_badge(&self, snapshot: &PushTarget, cancel: &CancellationToken) {
        if cancel.is_cancelled() {
            return;
        }
        let Some(target) = self.resolve_target(snapshot).await else {
            return;
        };
        if target.provider != "apns" || !target.badge {
            tracing::debug!(
                registration_id = %target.id,
                "push badge skipped; the registration no longer wants a badge"
            );
            return;
        }
        if cancel.is_cancelled() {
            return;
        }
        let Some(total) = read_badge_total(&self.store).await else {
            return;
        };
        let headers = ApnsHeaders {
            priority: "5",
            collapse: None,
        };
        let verdict = self
            .send_configured_apns(
                &target,
                Subject::Badge { total },
                headers,
                payload::apns_badge_body(total),
            )
            .await;
        if verdict == ProviderVerdict::Delete {
            self.delete_registration(target.id, "provider rejected the device token")
                .await;
        }
    }

    /// Tell an Android device that a room was read, so it cancels that room's
    /// notification (ADR 0129). APNs registrations send nothing.
    pub(crate) async fn deliver_room_read(
        &self,
        snapshot: &PushTarget,
        account_id: Uuid,
        room_id: &str,
        cancel: &CancellationToken,
    ) {
        if cancel.is_cancelled() {
            return;
        }
        let Some(target) = self.resolve_target(snapshot).await else {
            return;
        };
        if target.provider != "fcm" {
            return;
        }
        if cancel.is_cancelled() {
            return;
        }
        let collapse = payload::collapse_id(room_id);
        let body =
            payload::fcm_room_read_body(&target.token, &account_id.to_string(), room_id, &collapse);
        let verdict = self
            .send_configured_fcm(
                &target,
                Subject::Room {
                    account_id,
                    room_id,
                },
                body,
            )
            .await;
        if verdict == ProviderVerdict::Delete {
            self.delete_registration(target.id, "provider rejected the device token")
                .await;
        }
    }

    async fn resolve_target(&self, snapshot: &PushTarget) -> Option<PushTarget> {
        match tokio::time::timeout(LIVENESS_TIMEOUT, self.store.push_send_check(snapshot.id)).await
        {
            Ok(Ok(Some(check))) if !check.session_live => {
                self.delete_registration(snapshot.id, "session not live")
                    .await;
                None
            }
            Ok(Ok(Some(check))) => Some(check.target),
            Ok(Ok(None)) => None,
            Ok(Err(err)) => {
                tracing::warn!(
                    registration_id = %snapshot.id,
                    error = %err,
                    "push liveness check failed; sending"
                );
                Some(snapshot.clone())
            }
            Err(_) => {
                tracing::warn!(
                    registration_id = %snapshot.id,
                    "push liveness check timed out; sending"
                );
                Some(snapshot.clone())
            }
        }
    }

    async fn send_configured_apns(
        &self,
        target: &PushTarget,
        subject: Subject<'_>,
        headers: ApnsHeaders<'_>,
        body: Vec<u8>,
    ) -> ProviderVerdict {
        let Some(apns) = self.material.apns().cloned() else {
            self.warn_unconfigured("apns");
            return ProviderVerdict::Keep;
        };
        match tokio::time::timeout(
            PROVIDER_TIMEOUT,
            self.send_apns(&apns, target, subject, headers, body),
        )
        .await
        {
            Ok(Ok(verdict)) => verdict,
            Ok(Err(err)) => {
                log_send_failure("APNs", target.id, subject, &err.to_string());
                ProviderVerdict::Keep
            }
            Err(_) => {
                log_send_failure("APNs", target.id, subject, "timed out");
                ProviderVerdict::Keep
            }
        }
    }

    async fn send_configured_fcm(
        &self,
        target: &PushTarget,
        subject: Subject<'_>,
        body: Vec<u8>,
    ) -> ProviderVerdict {
        let Some(fcm) = self.material.fcm().cloned() else {
            self.warn_unconfigured("fcm");
            return ProviderVerdict::Keep;
        };
        match tokio::time::timeout(PROVIDER_TIMEOUT, self.send_fcm(&fcm, target, subject, body))
            .await
        {
            Ok(Ok(verdict)) => verdict,
            Ok(Err(err)) => {
                log_send_failure("FCM", target.id, subject, &err.to_string());
                ProviderVerdict::Keep
            }
            Err(_) => {
                log_send_failure("FCM", target.id, subject, "timed out");
                ProviderVerdict::Keep
            }
        }
    }

    fn warn_unconfigured(&self, provider: &str) {
        let flag = if provider == "apns" {
            &self.apns_warned
        } else {
            &self.fcm_warned
        };
        if flag.swap(true, Ordering::Relaxed) {
            tracing::debug!(provider, "push provider is not configured");
        } else {
            tracing::warn!(provider, "push provider is not configured; skipping sends");
        }
    }

    async fn delete_registration(&self, id: Uuid, why: &'static str) {
        match self.store.delete_push_registration_id(id).await {
            Ok(_) => tracing::info!(registration_id = %id, why, "push registration deleted"),
            Err(err) => tracing::warn!(
                registration_id = %id,
                error = %err,
                "failed to delete push registration"
            ),
        }
    }

    fn apns_jwt(&self, apns: &LoadedApns) -> Result<String, ProviderFailure> {
        let mut guard = self.apns_jwt.lock().expect("apns jwt lock");
        if let Some(cached) = guard.as_ref() {
            if cached.minted.elapsed() < APNS_JWT_TTL {
                return Ok(cached.value.clone());
            }
        }
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(apns.key_id.clone());
        let claims = serde_json::json!({
            "iss": apns.team_id,
            "iat": Utc::now().timestamp(),
        });
        let key = EncodingKey::from_ec_pem(&apns.private_key)
            .map_err(|_| ProviderFailure::transport("signing"))?;
        let value =
            encode(&header, &claims, &key).map_err(|_| ProviderFailure::transport("signing"))?;
        *guard = Some(CachedJwt {
            value: value.clone(),
            minted: Instant::now(),
        });
        Ok(value)
    }

    async fn fcm_bearer(&self, fcm: &LoadedFcm) -> Result<String, ProviderFailure> {
        if let Some(token) = self.fresh_fcm() {
            return Ok(token);
        }
        let _refresh = self.fcm_refresh.lock().await;
        if let Some(token) = self.fresh_fcm() {
            return Ok(token);
        }
        let now = Utc::now().timestamp();
        let header = Header::new(Algorithm::RS256);
        let claims = serde_json::json!({
            "iss": fcm.client_email,
            "scope": "https://www.googleapis.com/auth/firebase.messaging",
            "aud": "https://oauth2.googleapis.com/token",
            "iat": now,
            "exp": now + 3600,
        });
        let key = EncodingKey::from_rsa_pem(&fcm.private_key)
            .map_err(|_| ProviderFailure::transport("signing"))?;
        let assertion =
            encode(&header, &claims, &key).map_err(|_| ProviderFailure::transport("signing"))?;
        let form = format!(
            "grant_type={}&assertion={}",
            form_encode("urn:ietf:params:oauth:grant-type:jwt-bearer"),
            form_encode(&assertion)
        );
        let response = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .map_err(|err| ProviderFailure::transport(transport_kind(&err)))?;
        let status = response.status().as_u16();
        let body = limited_body(response).await?;
        if !(200..300).contains(&status) {
            return Err(ProviderFailure::status(status, parse_reason(&body).reason));
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| ProviderFailure::transport("token"))?;
        let value = parsed
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ProviderFailure::transport("token"))?
            .to_owned();
        let expires_in = parsed
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(3600);
        let slack = FCM_REFRESH_SLACK.min(Duration::from_secs(expires_in));
        *self.fcm_token.lock().expect("fcm token lock") = Some(CachedFcm {
            value: value.clone(),
            good_until: Instant::now() + Duration::from_secs(expires_in) - slack,
        });
        Ok(value)
    }

    fn fresh_fcm(&self) -> Option<String> {
        let guard = self.fcm_token.lock().expect("fcm token lock");
        guard
            .as_ref()
            .and_then(|cached| (Instant::now() < cached.good_until).then(|| cached.value.clone()))
    }

    async fn send_apns(
        &self,
        apns: &LoadedApns,
        target: &PushTarget,
        subject: Subject<'_>,
        headers: ApnsHeaders<'_>,
        body: Vec<u8>,
    ) -> Result<ProviderVerdict, ProviderFailure> {
        let host = match target.environment.as_deref() {
            Some("sandbox") => "https://api.sandbox.push.apple.com",
            Some("production") => "https://api.push.apple.com",
            _ => {
                tracing::warn!(
                    registration_id = %target.id,
                    "APNs registration has no environment; skipping"
                );
                return Ok(ProviderVerdict::Keep);
            }
        };
        let jwt = self.apns_jwt(apns)?;
        let url = format!("{host}/3/device/{}", form_encode(&target.token));
        let request_headers = apns_request_headers(&apns.bundle_id, headers);
        let (status, response_body) =
            post_json(&self.http, url, "bearer", jwt, &request_headers, body).await?;
        if apns_provider_token_rejected(status, &response_body) {
            self.invalidate_apns_jwt();
        }
        log_provider_response("APNs", target.id, subject, status, &response_body);
        Ok(apns_verdict(status, &response_body, target.updated_at))
    }

    fn invalidate_apns_jwt(&self) {
        *self.apns_jwt.lock().expect("apns jwt lock") = None;
    }

    fn invalidate_fcm_token(&self) {
        *self.fcm_token.lock().expect("fcm token lock") = None;
    }

    async fn send_fcm(
        &self,
        fcm: &LoadedFcm,
        target: &PushTarget,
        subject: Subject<'_>,
        body: Vec<u8>,
    ) -> Result<ProviderVerdict, ProviderFailure> {
        let bearer = self.fcm_bearer(fcm).await?;
        let url = format!(
            "https://fcm.googleapis.com/v1/projects/{}/messages:send",
            fcm.project_id
        );
        // Google ignores a lowercase scheme and answers 401 UNAUTHENTICATED.
        // Apple accepts the lowercase scheme used for APNs.
        let (status, response_body) =
            post_json(&self.http, url, "Bearer", bearer, &[], body).await?;
        if fcm_bearer_rejected(status) {
            self.invalidate_fcm_token();
        }
        log_provider_response("FCM", target.id, subject, status, &response_body);
        Ok(fcm_verdict(status, &response_body))
    }
}

/// The APNs request headers, in a fixed order. A badge-only push has no
/// collapse id.
fn apns_request_headers<'a>(topic: &'a str, headers: ApnsHeaders<'a>) -> Vec<(&'a str, &'a str)> {
    let mut out = vec![
        ("apns-topic", topic),
        ("apns-push-type", "alert"),
        ("apns-priority", headers.priority),
    ];
    if let Some(collapse) = headers.collapse {
        out.push(("apns-collapse-id", collapse));
    }
    out
}

fn log_send_failure(provider: &str, registration_id: Uuid, subject: Subject<'_>, error: &str) {
    match subject {
        Subject::Room {
            account_id,
            room_id,
        } => tracing::warn!(
            provider,
            registration_id = %registration_id,
            %account_id,
            room_id,
            error,
            "push send failed"
        ),
        Subject::Badge { total } => tracing::warn!(
            provider,
            registration_id = %registration_id,
            total,
            error,
            "push badge send failed"
        ),
    }
}

fn log_provider_response(
    provider: &str,
    registration_id: Uuid,
    subject: Subject<'_>,
    status: u16,
    body: &[u8],
) {
    if (200..300).contains(&status) {
        match subject {
            Subject::Room {
                account_id,
                room_id,
            } => tracing::debug!(
                provider,
                registration_id = %registration_id,
                %account_id,
                room_id,
                status,
                "push send accepted"
            ),
            Subject::Badge { total } => tracing::debug!(
                provider,
                registration_id = %registration_id,
                total,
                status,
                "push badge send accepted"
            ),
        }
        return;
    }
    let reason = parse_reason(body).reason;
    let reason = reason.as_deref().unwrap_or("");
    match subject {
        Subject::Room {
            account_id,
            room_id,
        } => tracing::warn!(
            provider,
            registration_id = %registration_id,
            %account_id,
            room_id,
            status,
            reason,
            "push send was not accepted"
        ),
        Subject::Badge { total } => tracing::warn!(
            provider,
            registration_id = %registration_id,
            total,
            status,
            reason,
            "push badge send was not accepted"
        ),
    }
}

/// The instance unread total, or `None` when it cannot be read in time.
///
/// Bounded like the liveness check. The caller skips the badge on `None`.
async fn read_badge_total(store: &Store) -> Option<u64> {
    match tokio::time::timeout(LIVENESS_TIMEOUT, store.unread_badge_total()).await {
        Ok(Ok(total)) => Some(total),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "push badge total unavailable; badge skipped");
            None
        }
        Err(_) => {
            tracing::warn!("push badge total timed out; badge skipped");
            None
        }
    }
}

fn push_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PROVIDER_TIMEOUT)
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("push HTTP client must build")
}

async fn post_json(
    http: &reqwest::Client,
    url: String,
    scheme: &str,
    bearer: String,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Result<(u16, Vec<u8>), ProviderFailure> {
    let mut request = http
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, format!("{scheme} {bearer}"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .send()
        .await
        .map_err(|err| ProviderFailure::transport(transport_kind(&err)))?;
    let status = response.status().as_u16();
    let body = limited_body(response).await?;
    Ok((status, body))
}

async fn limited_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderFailure> {
    let bytes = response
        .bytes()
        .await
        .map_err(|err| ProviderFailure::transport(transport_kind(&err)))?;
    if bytes.len() > BODY_CAP {
        return Ok(Vec::new());
    }
    Ok(bytes.to_vec())
}

/// One name lookup, and one badge total read, shared by every device that
/// receives the same message.
pub(crate) struct SharedBanner {
    account_id: Uuid,
    room_id: String,
    sender: String,
    once: tokio::sync::OnceCell<(Option<String>, String)>,
    badge: tokio::sync::OnceCell<Option<u64>>,
}

impl SharedBanner {
    pub(crate) fn new(account_id: Uuid, room_id: &str, sender: &str) -> Self {
        Self {
            account_id,
            room_id: room_id.to_owned(),
            sender: sender.to_owned(),
            once: tokio::sync::OnceCell::new(),
            badge: tokio::sync::OnceCell::new(),
        }
    }

    async fn resolve(&self, store: &Store) -> (Option<String>, String) {
        self.once
            .get_or_init(|| banner_names(store, self.account_id, &self.room_id, &self.sender))
            .await
            .clone()
    }

    /// The unread total for this alert's `aps.badge`. Read by the first
    /// badge-enabled device to need it. A failed read leaves the badge off the
    /// alert, which still sends.
    async fn badge_total(&self, store: &Store) -> Option<u64> {
        *self.badge.get_or_init(|| read_badge_total(store)).await
    }
}

/// `true` when APNs rejected the provider JWT itself. The cached token must
/// be dropped so the next wake mints a new one. The registration stays.
fn apns_provider_token_rejected(status: u16, body: &[u8]) -> bool {
    status == 403
        && matches!(
            parse_reason(body).reason.as_deref(),
            Some("ExpiredProviderToken" | "InvalidProviderToken")
        )
}

/// Names for the banner. A failed or blank read uses the fallback and still sends.
async fn banner_names(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    sender: &str,
) -> (Option<String>, String) {
    let (room_name, member_name) = tokio::join!(
        tokio::time::timeout(
            LIVENESS_TIMEOUT,
            store.room_display_name(account_id, room_id)
        ),
        tokio::time::timeout(
            LIVENESS_TIMEOUT,
            store.member_display_name(account_id, room_id, sender)
        ),
    );
    let room_name = match room_name {
        Ok(Ok(name)) => name,
        Ok(Err(err)) => {
            tracing::debug!(%account_id, room_id, error = %err, "push room name unavailable");
            None
        }
        Err(_) => None,
    };
    let sender_display = match member_name {
        Ok(Ok(Some(name))) => name,
        Ok(Err(err)) => {
            tracing::debug!(%account_id, room_id, error = %err, "push sender name unavailable");
            at_localpart(sender)
        }
        _ => at_localpart(sender),
    };
    (room_name, sender_display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::Arc;

    fn updated() -> DateTime<Utc> {
        Utc.timestamp_millis_opt(1_000).single().unwrap()
    }

    #[test]
    fn apns_deletes_only_a_dead_device() {
        let updated_at = updated();
        assert_eq!(
            apns_verdict(200, br#"{"reason":"Unregistered"}"#, updated_at),
            ProviderVerdict::Sent
        );
        assert_eq!(
            apns_verdict(
                410,
                br#"{"reason":"Unregistered","timestamp":2000}"#,
                updated_at
            ),
            ProviderVerdict::Delete
        );
        assert_eq!(
            apns_verdict(
                410,
                br#"{"reason":"Unregistered","timestamp":1000}"#,
                updated_at
            ),
            ProviderVerdict::Keep
        );
        assert_eq!(
            apns_verdict(410, br#"{"reason":"Unregistered"}"#, updated_at),
            ProviderVerdict::Keep
        );
        assert_eq!(
            apns_verdict(400, br#"{"reason":"BadDeviceToken"}"#, updated_at),
            ProviderVerdict::Delete
        );
        assert_eq!(
            apns_verdict(400, br#"{"reason":"PayloadTooLarge"}"#, updated_at),
            ProviderVerdict::Keep
        );
        assert_eq!(
            apns_verdict(400, br#"{"reason":"BadTopic"}"#, updated_at),
            ProviderVerdict::Keep
        );
        assert_eq!(apns_verdict(429, b"", updated_at), ProviderVerdict::Keep);
        assert!(apns_provider_token_rejected(
            403,
            br#"{"reason":"ExpiredProviderToken"}"#
        ));
        assert!(apns_provider_token_rejected(
            403,
            br#"{"reason":"InvalidProviderToken"}"#
        ));
        assert!(!apns_provider_token_rejected(
            403,
            br#"{"reason":"BadDeviceToken"}"#
        ));
        assert!(!apns_provider_token_rejected(
            400,
            br#"{"reason":"ExpiredProviderToken"}"#
        ));
    }

    #[test]
    fn fcm_deletes_only_an_unregistered_token() {
        assert_eq!(
            fcm_verdict(404, br#"{"error":{"status":"UNREGISTERED"}}"#),
            ProviderVerdict::Delete
        );
        assert_eq!(
            fcm_verdict(400, br#"{"error":{"status":"INVALID_ARGUMENT"}}"#),
            ProviderVerdict::Keep
        );
        assert_eq!(
            fcm_verdict(403, br#"{"error":{"status":"SENDER_ID_MISMATCH"}}"#),
            ProviderVerdict::Keep
        );
        assert_eq!(
            fcm_verdict(429, br#"{"error":{"status":"QUOTA_EXCEEDED"}}"#),
            ProviderVerdict::Keep
        );
        assert_eq!(fcm_verdict(404, b"not json"), ProviderVerdict::Keep);
        assert!(fcm_bearer_rejected(401));
        assert!(!fcm_bearer_rejected(403));
        assert_eq!(
            fcm_verdict(401, br#"{"error":{"status":"UNAUTHENTICATED"}}"#),
            ProviderVerdict::Keep
        );
    }

    #[test]
    fn a_provider_error_does_not_include_a_token_or_a_url() {
        let text = ProviderFailure::status(410, Some("Unregistered".to_owned())).to_string();
        assert!(!text.contains("device-token-sentinel"));
        assert!(!text.contains("https://"));
        assert_eq!(
            ProviderFailure::transport("timeout").to_string(),
            "push provider timeout"
        );
    }

    #[tokio::test]
    async fn a_rejected_send_is_not_retried() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/3/device/{token}",
                axum::routing::post(
                    move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                        let seen = seen.clone();
                        async move {
                            seen.fetch_add(1, Ordering::Relaxed);
                            assert_eq!(headers.get("apns-push-type").unwrap(), "alert");
                            assert_eq!(headers.get("apns-priority").unwrap(), "10");
                            assert_eq!(headers.get("apns-topic").unwrap(), "org.matrixaxon.axon");
                            assert_eq!(headers.get("apns-collapse-id").unwrap().len(), 32);
                            assert!(headers
                                .get(axum::http::header::AUTHORIZATION)
                                .unwrap()
                                .to_str()
                                .unwrap()
                                .starts_with("bearer "));
                            let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(parsed["aps"]["alert"]["body"], "New activity");
                            assert!(parsed.get("thread_root_id").is_none());
                            assert!(!body
                                .windows(18)
                                .any(|window| window == b"device-token-value"));
                            (axum::http::StatusCode::TOO_MANY_REQUESTS, "")
                        }
                    },
                ),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let http = push_http_client();
        let url = format!("http://{address}/3/device/abc");
        let body = br#"{"aps":{"alert":{"title":"Axon","body":"New activity"}},"account_id":"a","room_id":"!r","event_id":"$e"}"#.to_vec();
        let (status, response_body) = post_json(
            &http,
            url,
            "bearer",
            "jwt".to_owned(),
            &[
                ("apns-topic", "org.matrixaxon.axon"),
                ("apns-push-type", "alert"),
                ("apns-priority", "10"),
                ("apns-collapse-id", "0123456789abcdef0123456789abcdef"),
            ],
            body,
        )
        .await
        .unwrap();
        assert_eq!(status, 429);
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        assert_eq!(
            apns_verdict(status, &response_body, updated()),
            ProviderVerdict::Keep
        );
    }
}
