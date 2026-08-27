//! Matrix OAuth client registration and durable reuse (ADR 0097).
//!
//! Registration is authorization-server state, not account state: one public
//! client ID is safely shared by every account using the same discovered
//! issuer, while access and refresh tokens remain encrypted per account.

use std::{sync::Arc, time::Duration};

use axon_core::MatrixOAuthConfig;
use axon_store::{MatrixOAuthRegistration, Store, StoreError};
use matrix_sdk::{
    authentication::oauth::{
        registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
        ClientId, OAuthSession,
    },
    ruma::{
        api::client::discovery::get_authorization_server_metadata::v1::AuthorizationServerMetadata,
        serde::Raw,
    },
    Client, SessionChange,
};
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::error::SyncError;

const MAX_STATIC_REGISTRATIONS: usize = 64;
const MAX_SERVER_URL_BYTES: usize = 2_048;
const MAX_CLIENT_ID_BYTES: usize = 512;
const MAX_OAUTH_METADATA_BYTES: usize = 64 * 1024;
const MAX_REGISTRATION_RESPONSE_BYTES: usize = 64 * 1024;
const PERSIST_RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
    Duration::from_secs(5),
];
const SHUTDOWN_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// Serializes the low-frequency lookup/register/write critical section.
///
/// A single lock is intentionally used instead of a per-issuer lock map: OAuth
/// registration happens only while starting a login flow, and a global lock is
/// naturally bounded while still preventing duplicate dynamic registrations.
#[derive(Clone)]
pub struct MatrixOAuthRegistrationManager {
    store: Store,
    config: MatrixOAuthConfig,
    registration_lock: Arc<Mutex<()>>,
}

impl MatrixOAuthRegistrationManager {
    pub fn new(store: Store, config: MatrixOAuthConfig) -> Self {
        Self {
            store,
            config,
            registration_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Discover the issuer, restore a durable/static public client ID, or
    /// dynamically register Axon and persist the result.
    pub async fn prepare(&self, client: &Client) -> Result<ClientId, SyncError> {
        if client.oauth().client_id().is_some() {
            return Err(SyncError::MatrixOAuthLocalState);
        }
        validate_config(&self.config)?;
        let timeout = Duration::from_secs(self.config.request_timeout_secs);
        let http = reqwest::Client::builder()
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SyncError::MatrixOAuthConfiguration)?;
        let metadata = discover_metadata(&http, &client.homeserver(), timeout).await?;
        let issuer = metadata.issuer;
        let issuer_url = issuer.as_str();
        let homeserver = client.homeserver();
        if issuer_url.len() > MAX_SERVER_URL_BYTES
            || homeserver.as_str().len() > MAX_SERVER_URL_BYTES
        {
            return Err(SyncError::Sdk(
                "Matrix OAuth server URL is too long".to_owned(),
            ));
        }

        // Re-check durable state only after entering the critical section. Two
        // concurrent flows may discover together, but only one can register.
        let _guard = self.registration_lock.lock().await;
        if let Some(registration) = self.store.matrix_oauth_registration(issuer_url).await? {
            validate_client_id(&registration.client_id)
                .map_err(|_| SyncError::MatrixOAuthLocalState)?;
            let client_id = ClientId::new(registration.client_id);
            client.oauth().restore_registered_client(client_id.clone());
            tracing::debug!("restored persisted Matrix OAuth client registration");
            return Ok(client_id);
        }

        if let Some(client_id) = static_client_id(&self.config, &homeserver, &issuer)? {
            persist_then_restore_registration(
                client,
                &client_id,
                self.persist(&issuer, &homeserver, &client_id),
            )
            .await?;
            tracing::info!("restored operator-provided Matrix OAuth client registration");
            return Ok(client_id);
        }

        let registration_endpoint = metadata.registration_endpoint.ok_or_else(|| {
            SyncError::Sdk(
                "Matrix OAuth server does not advertise dynamic client registration and no static client ID matches"
                    .to_owned(),
            )
        })?;
        let client_metadata = client_metadata().map_err(|_| SyncError::MatrixOAuthLocalState)?;
        let body = client_metadata.json().get().as_bytes().to_vec();
        let response = http
            .post(registration_endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| {
                oauth_http_error("oauth_registration", "dynamic registration", timeout, err)
            })?;
        if !response.status().is_success() {
            tracing::warn!(
                phase = "oauth_registration",
                error_class = "upstream",
                http_status = response.status().as_u16(),
                "Matrix OAuth dynamic registration failed"
            );
            return Err(SyncError::Sdk(format!(
                "Matrix OAuth dynamic registration returned HTTP {}",
                response.status()
            )));
        }
        let body = bounded_body(response, MAX_REGISTRATION_RESPONSE_BYTES).await?;
        let response: matrix_sdk::authentication::oauth::registration::ClientRegistrationResponse =
            serde_json::from_slice(&body).map_err(|err| {
                tracing::warn!(
                    phase = "oauth_registration",
                    error_class = "invalid_response",
                    "Matrix OAuth dynamic registration response was invalid"
                );
                SyncError::Sdk(format!("invalid Matrix OAuth registration response: {err}"))
            })?;
        validate_client_id(response.client_id.as_str()).inspect_err(|_| {
            tracing::warn!(
                phase = "oauth_registration",
                error_class = "invalid_response",
                "Matrix OAuth dynamic registration returned an invalid client ID"
            );
        })?;
        persist_then_restore_registration(
            client,
            &response.client_id,
            self.persist(&issuer, &homeserver, &response.client_id),
        )
        .await?;
        tracing::info!("dynamically registered Matrix OAuth client");
        Ok(response.client_id)
    }

    async fn persist(
        &self,
        issuer: &Url,
        homeserver: &Url,
        client_id: &ClientId,
    ) -> Result<(), SyncError> {
        validate_client_id(client_id.as_str()).map_err(|_| SyncError::MatrixOAuthLocalState)?;
        self.store
            .upsert_matrix_oauth_registration(&MatrixOAuthRegistration {
                issuer_url: issuer.to_string(),
                homeserver_url: homeserver.to_string(),
                client_id: client_id.as_str().to_owned(),
            })
            .await?;
        Ok(())
    }
}

async fn persist_then_restore_registration(
    client: &Client,
    client_id: &ClientId,
    persist: impl std::future::Future<Output = Result<(), SyncError>>,
) -> Result<(), SyncError> {
    persist.await?;
    client.oauth().restore_registered_client(client_id.clone());
    Ok(())
}

async fn discover_metadata(
    http: &reqwest::Client,
    homeserver: &Url,
    timeout: Duration,
) -> Result<AuthorizationServerMetadata, SyncError> {
    let stable = homeserver
        .join("/_matrix/client/v1/auth_metadata")
        .map_err(|err| SyncError::Sdk(format!("invalid Matrix OAuth discovery URL: {err}")))?;
    let unstable = homeserver
        .join("/_matrix/client/unstable/org.matrix.msc2965/auth_metadata")
        .map_err(|err| SyncError::Sdk(format!("invalid Matrix OAuth discovery URL: {err}")))?;

    let mut response = http
        .get(stable)
        .send()
        .await
        .map_err(|err| oauth_http_error("oauth_discovery", "discovery", timeout, err))?;
    let stable_status = response.status();
    if stable_status == reqwest::StatusCode::NOT_FOUND {
        response = http
            .get(unstable)
            .send()
            .await
            .map_err(|err| oauth_http_error("oauth_discovery", "discovery", timeout, err))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            tracing::warn!(
                phase = "oauth_discovery",
                error_class = "unsupported",
                stable_http_status = stable_status.as_u16(),
                unstable_http_status = response.status().as_u16(),
                "homeserver does not advertise Matrix OAuth authentication"
            );
            return Err(SyncError::MatrixOAuthUnavailable);
        }
    }
    if !response.status().is_success() {
        tracing::warn!(
            phase = "oauth_discovery",
            error_class = "upstream",
            endpoint = if stable_status == reqwest::StatusCode::NOT_FOUND {
                "unstable"
            } else {
                "stable"
            },
            http_status = response.status().as_u16(),
            "Matrix OAuth discovery failed"
        );
        return Err(SyncError::Sdk(format!(
            "Matrix OAuth discovery returned HTTP {}",
            response.status()
        )));
    }
    let body = bounded_body(response, MAX_OAUTH_METADATA_BYTES).await?;
    let raw: Raw<AuthorizationServerMetadata> = serde_json::from_slice(&body).map_err(|err| {
        tracing::warn!(
            phase = "oauth_discovery",
            error_class = "invalid_response",
            "Matrix OAuth discovery response was invalid"
        );
        SyncError::Sdk(format!("invalid Matrix OAuth metadata: {err}"))
    })?;
    let metadata = raw.deserialize().map_err(|err| {
        tracing::warn!(
            phase = "oauth_discovery",
            error_class = "invalid_metadata",
            "Matrix OAuth discovery metadata was invalid"
        );
        SyncError::Sdk(format!("invalid Matrix OAuth metadata: {err}"))
    })?;
    validate_metadata_urls(homeserver, &metadata).inspect_err(|_| {
        tracing::warn!(
            phase = "oauth_discovery",
            error_class = "unsafe_metadata",
            "Matrix OAuth discovery metadata contains unsafe URLs"
        );
    })?;
    Ok(metadata)
}

fn validate_metadata_urls(
    homeserver: &Url,
    metadata: &AuthorizationServerMetadata,
) -> Result<(), SyncError> {
    if homeserver.scheme() == "http" {
        metadata
            .insecure_validate_urls()
            .map_err(|err| SyncError::Sdk(format!("invalid Matrix OAuth metadata: {err}")))?;
    } else {
        metadata
            .validate_urls()
            .map_err(|err| SyncError::Sdk(format!("invalid Matrix OAuth metadata: {err}")))?;
    }
    Ok(())
}

async fn bounded_body(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, SyncError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(SyncError::Sdk(format!(
            "Matrix OAuth response exceeds the {max_bytes}-byte limit"
        )));
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(max_bytes as u64) as usize,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| SyncError::Sdk(format!("reading Matrix OAuth response: {err}")))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(SyncError::Sdk(format!(
                "Matrix OAuth response exceeds the {max_bytes}-byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn oauth_http_error(
    phase: &'static str,
    operation: &str,
    timeout: Duration,
    err: reqwest::Error,
) -> SyncError {
    if err.is_timeout() {
        tracing::warn!(
            phase,
            error_class = "timeout",
            "Matrix OAuth request timed out"
        );
        SyncError::Sdk(format!(
            "Matrix OAuth {operation} timed out after {} seconds",
            timeout.as_secs()
        ))
    } else {
        tracing::warn!(
            phase,
            error_class = "network",
            "Matrix OAuth request failed"
        );
        SyncError::Sdk(format!("Matrix OAuth {operation} request failed: {err}"))
    }
}

fn validate_client_id(client_id: &str) -> Result<(), SyncError> {
    if client_id.is_empty() || client_id.len() > MAX_CLIENT_ID_BYTES {
        return Err(SyncError::Sdk(
            "Matrix OAuth client_id has an invalid length".to_owned(),
        ));
    }
    Ok(())
}

fn validate_config(config: &MatrixOAuthConfig) -> Result<(), SyncError> {
    if config.request_timeout_secs == 0 {
        return Err(SyncError::MatrixOAuthConfiguration);
    }
    if config.static_registrations.len() > MAX_STATIC_REGISTRATIONS {
        return Err(SyncError::MatrixOAuthConfiguration);
    }
    for registration in config.static_registrations.values() {
        if registration.server_url.len() > MAX_SERVER_URL_BYTES {
            return Err(SyncError::MatrixOAuthConfiguration);
        }
        validate_client_id(&registration.client_id)
            .map_err(|_| SyncError::MatrixOAuthConfiguration)?;
    }
    Ok(())
}

fn static_client_id(
    config: &MatrixOAuthConfig,
    homeserver: &Url,
    issuer: &Url,
) -> Result<Option<ClientId>, SyncError> {
    for registration in config.static_registrations.values() {
        let server_url = Url::parse(&registration.server_url)
            .map_err(|_| SyncError::MatrixOAuthConfiguration)?;
        if server_url == *homeserver || server_url == *issuer {
            return Ok(Some(ClientId::new(registration.client_id.clone())));
        }
    }
    Ok(None)
}

fn client_metadata() -> Result<Raw<ClientMetadata>, SyncError> {
    let client_uri = Url::parse("https://github.com/matrix-axon/matrix-axon")
        .map_err(|err| SyncError::Sdk(format!("invalid built-in OAuth client URI: {err}")))?;
    let mut metadata = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::DeviceCode],
        Localized::new(client_uri, None),
    );
    metadata.client_name = Some(Localized::new("Axon".to_owned(), None));
    Raw::new(&metadata)
        .map_err(|err| SyncError::Sdk(format!("serializing OAuth client metadata: {err}")))
}

/// Persist the newest OAuth session after every SDK refresh notification.
///
/// The caller creates `changes` before starting sync, closing the normal
/// subscribe/refresh race. The initial full snapshot also heals a refresh that
/// happened earlier through a lazily connected gateway client. Only this one
/// task writes refresh snapshots for an account run, so an older retry cannot
/// overwrite a newer rotation.
pub(crate) async fn watch_session_changes(
    client: Client,
    store: Store,
    account_id: Uuid,
    store_key: String,
    mut changes: broadcast::Receiver<SessionChange>,
    cancel: CancellationToken,
) {
    let _ = persist_latest_with_retry(&client, &store, account_id, &store_key, &cancel).await;

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            change = changes.recv() => match change {
                Ok(SessionChange::TokensRefreshed) => {
                    if persist_latest_with_retry(
                        &client,
                        &store,
                        account_id,
                        &store_key,
                        &cancel,
                    ).await {
                        tracing::info!(
                            %account_id,
                            "Matrix OAuth tokens refreshed and persisted"
                        );
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = persist_latest_with_retry(
                        &client,
                        &store,
                        account_id,
                        &store_key,
                        &cancel,
                    ).await;
                }
                Ok(SessionChange::UnknownToken(_)) => {
                    tracing::warn!(
                        %account_id,
                        "Matrix OAuth session was rejected; account requires a fresh login"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }

    // Revocation and process shutdown can race a final refresh. Flush the
    // newest in-memory snapshot, but never let database trouble wedge teardown.
    let flush_cancel = CancellationToken::new();
    let flush = persist_latest_with_retry(&client, &store, account_id, &store_key, &flush_cancel);
    if tokio::time::timeout(SHUTDOWN_FLUSH_TIMEOUT, flush)
        .await
        .is_err()
    {
        tracing::warn!(
            %account_id,
            timeout_secs = SHUTDOWN_FLUSH_TIMEOUT.as_secs(),
            "timed out flushing Matrix OAuth session during shutdown"
        );
    }
}

async fn persist_latest_with_retry(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    store_key: &str,
    cancel: &CancellationToken,
) -> bool {
    let mut degraded = false;
    for (attempt, retry_delay) in PERSIST_RETRY_DELAYS
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None))
        .enumerate()
    {
        let Some(session) = client.oauth().full_session() else {
            tracing::warn!(%account_id, "OAuth session snapshot is unavailable");
            return false;
        };
        match persist_snapshot(store, account_id, store_key, session).await {
            Ok(()) => {
                if degraded {
                    tracing::info!(
                        %account_id,
                        "Matrix OAuth session durability recovered"
                    );
                }
                return true;
            }
            Err(SyncError::Store(StoreError::OAuthSessionNotCurrent)) => {
                tracing::debug!(
                    %account_id,
                    "stopped persisting an OAuth session that is no longer current"
                );
                return false;
            }
            Err(err) => match retry_delay {
                Some(delay) => {
                    if !degraded {
                        degraded = true;
                        tracing::warn!(
                            %account_id,
                            error = %err,
                            "Matrix OAuth session durability degraded; retrying latest snapshot"
                        );
                    }
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return false,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                None => {
                    tracing::error!(
                        %account_id,
                        error = %err,
                        attempts = attempt + 1,
                        "failed to persist refreshed Matrix OAuth session; a crash may require fresh login"
                    );
                    return false;
                }
            },
        }
    }
    false
}

async fn persist_snapshot(
    store: &Store,
    account_id: Uuid,
    store_key: &str,
    session: OAuthSession,
) -> Result<(), SyncError> {
    let refresh_token = session.user.tokens.refresh_token.ok_or_else(|| {
        SyncError::Sdk("Matrix OAuth session snapshot has no refresh token".to_owned())
    })?;
    store
        .update_account_oauth_session(
            account_id,
            &session.user.tokens.access_token,
            &refresh_token,
            session.client_id.as_str(),
            store_key,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_core::MatrixOAuthStaticRegistration;
    use axum::{
        extract::State,
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn static_registration_matches_normalized_homeserver_or_issuer() {
        let config = MatrixOAuthConfig {
            static_registrations: std::collections::BTreeMap::from([(
                "mas".to_owned(),
                MatrixOAuthStaticRegistration {
                    server_url: "https://issuer.example".to_owned(),
                    client_id: "static-client".to_owned(),
                },
            )]),
            ..MatrixOAuthConfig::default()
        };
        let client_id = static_client_id(
            &config,
            &Url::parse("https://hs.example/").unwrap(),
            &Url::parse("https://issuer.example/").unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(client_id.as_str(), "static-client");
    }

    #[test]
    fn static_registration_limits_fail_before_network_use() {
        let config = MatrixOAuthConfig {
            request_timeout_secs: 0,
            ..MatrixOAuthConfig::default()
        };
        assert!(matches!(
            validate_config(&config),
            Err(SyncError::MatrixOAuthConfiguration)
        ));

        let config = MatrixOAuthConfig {
            static_registrations: (0..=MAX_STATIC_REGISTRATIONS)
                .map(|n| {
                    (
                        format!("registration-{n}"),
                        MatrixOAuthStaticRegistration {
                            server_url: format!("https://issuer-{n}.example/"),
                            client_id: format!("client-{n}"),
                        },
                    )
                })
                .collect(),
            ..MatrixOAuthConfig::default()
        };
        assert!(matches!(
            validate_config(&config),
            Err(SyncError::MatrixOAuthConfiguration)
        ));
    }

    #[test]
    fn client_metadata_requests_device_code_with_refresh() {
        let raw = client_metadata().unwrap();
        let json: serde_json::Value = serde_json::from_str(raw.json().get()).unwrap();
        assert_eq!(json["application_type"], "native");
        assert_eq!(json["client_name"], "Axon");
        assert_eq!(
            json["grant_types"],
            serde_json::json!([
                "refresh_token",
                "urn:ietf:params:oauth:grant-type:device_code"
            ])
        );
    }

    #[test]
    fn secure_homeserver_rejects_insecure_authorization_server_metadata() {
        let raw: Raw<AuthorizationServerMetadata> = serde_json::from_value(serde_json::json!({
            "issuer": "http://auth.example.org/",
            "authorization_endpoint": "http://auth.example.org/authorize",
            "token_endpoint": "http://auth.example.org/token",
            "response_types_supported": ["code"],
            "response_modes_supported": ["query", "fragment"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "revocation_endpoint": "http://auth.example.org/revoke",
            "code_challenge_methods_supported": ["S256"],
            "device_authorization_endpoint": "http://auth.example.org/device"
        }))
        .unwrap();
        let metadata = raw.deserialize().unwrap();
        let error = validate_metadata_urls(
            &Url::parse("https://homeserver.example.org/").unwrap(),
            &metadata,
        )
        .unwrap_err();

        assert!(error.to_string().to_lowercase().contains("https"));
    }

    #[derive(Clone)]
    struct OAuthMockState {
        base_url: String,
        registration_calls: Arc<AtomicUsize>,
    }

    async fn versions() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "versions": ["v1.15"],
            "unstable_features": {}
        }))
    }

    async fn auth_metadata(State(state): State<OAuthMockState>) -> Json<serde_json::Value> {
        let base = state.base_url.trim_end_matches('/');
        Json(serde_json::json!({
            "issuer": format!("{base}/"),
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": format!("{base}/register"),
            "response_types_supported": ["code"],
            "response_modes_supported": ["query", "fragment"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "revocation_endpoint": format!("{base}/revoke"),
            "code_challenge_methods_supported": ["S256"],
            "device_authorization_endpoint": format!("{base}/device")
        }))
    }

    async fn register_client(
        State(state): State<OAuthMockState>,
        Json(metadata): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        assert_eq!(metadata["application_type"], "native");
        assert_eq!(metadata["client_name"], "Axon");
        state.registration_calls.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({ "client_id": "dynamic-client" }))
    }

    async fn refresh_token() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "token_type": "Bearer",
            "expires_in": 300
        }))
    }

    #[tokio::test]
    async fn oversized_discovery_metadata_is_rejected() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/_matrix/client/v1/auth_metadata",
            get(|| async { vec![b'x'; MAX_OAUTH_METADATA_BYTES + 1] }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let err = discover_metadata(
            &http,
            &Url::parse(&base_url).unwrap(),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("65536-byte limit"));
        server.abort();
    }

    #[tokio::test]
    async fn missing_stable_and_unstable_metadata_is_unsupported() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/_matrix/client/v1/auth_metadata",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/_matrix/client/unstable/org.matrix.msc2965/auth_metadata",
                get(|| async { StatusCode::NOT_FOUND }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let error = discover_metadata(
            &http,
            &Url::parse(&base_url).unwrap(),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, SyncError::MatrixOAuthUnavailable));
        server.abort();
    }

    #[tokio::test]
    async fn persistence_failure_leaves_registration_retryable() {
        let client = Client::builder()
            .homeserver_url("https://homeserver.example.org/")
            .build()
            .await
            .unwrap();
        let client_id = ClientId::new("public-client".to_owned());

        let error = persist_then_restore_registration(&client, &client_id, async {
            Err(SyncError::Store(StoreError::InvalidAccountSession(
                "forced persistence failure".to_owned(),
            )))
        })
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            SyncError::Store(StoreError::InvalidAccountSession(_))
        ));
        assert!(client.oauth().client_id().is_none());

        persist_then_restore_registration(&client, &client_id, async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(
            client.oauth().client_id().unwrap().as_str(),
            client_id.as_str()
        );
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn dynamic_registration_is_persisted_and_reused_by_issuer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/", listener.local_addr().unwrap());
        let registration_calls = Arc::new(AtomicUsize::new(0));
        let state = OAuthMockState {
            base_url: base_url.clone(),
            registration_calls: registration_calls.clone(),
        };
        let app = Router::new()
            .route("/_matrix/client/versions", get(versions))
            .route("/_matrix/client/v1/auth_metadata", get(auth_metadata))
            .route(
                "/_matrix/client/unstable/org.matrix.msc2965/auth_metadata",
                get(auth_metadata),
            )
            .route("/register", post(register_client))
            .route("/token", post(refresh_token))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let store = Store::connect(&database_url, 5).await.unwrap();
        sqlx_core::query::query("DELETE FROM matrix_oauth_registrations WHERE issuer_url = $1")
            .bind(&base_url)
            .execute(store.pool())
            .await
            .unwrap();
        let registrations =
            MatrixOAuthRegistrationManager::new(store.clone(), MatrixOAuthConfig::default());

        let first = Client::builder()
            .homeserver_url(&base_url)
            .handle_refresh_tokens()
            .build()
            .await
            .unwrap();
        let client_id = registrations.prepare(&first).await.unwrap();
        assert_eq!(client_id.as_str(), "dynamic-client");
        assert_eq!(registration_calls.load(Ordering::SeqCst), 1);

        let second = Client::builder()
            .homeserver_url(&base_url)
            .handle_refresh_tokens()
            .build()
            .await
            .unwrap();
        let client_id = registrations.prepare(&second).await.unwrap();
        assert_eq!(client_id.as_str(), "dynamic-client");
        assert_eq!(registration_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            second.oauth().client_id().unwrap().as_str(),
            "dynamic-client"
        );

        let persisted = store
            .matrix_oauth_registration(&base_url)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.homeserver_url, base_url);
        assert_eq!(persisted.client_id, "dynamic-client");

        sqlx_core::query::query("DELETE FROM matrix_oauth_registrations WHERE issuer_url = $1")
            .bind(&persisted.issuer_url)
            .execute(store.pool())
            .await
            .unwrap();
        let static_registrations = MatrixOAuthRegistrationManager::new(
            store.clone(),
            MatrixOAuthConfig {
                static_registrations: std::collections::BTreeMap::from([(
                    "test-mas".to_owned(),
                    MatrixOAuthStaticRegistration {
                        server_url: base_url.clone(),
                        client_id: "static-client".to_owned(),
                    },
                )]),
                ..MatrixOAuthConfig::default()
            },
        );
        let static_client = Client::builder()
            .homeserver_url(&base_url)
            .build()
            .await
            .unwrap();
        let static_client_id = static_registrations.prepare(&static_client).await.unwrap();
        assert_eq!(static_client_id.as_str(), "static-client");
        assert_eq!(registration_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .matrix_oauth_registration(&base_url)
                .await
                .unwrap()
                .unwrap()
                .client_id,
            "static-client"
        );

        // A real SDK refresh signal rotates both encrypted tokens through the
        // supervised persister, and the resulting snapshot is restart-ready.
        let user_id = format!("@oauth-{}:example.org", Uuid::new_v4());
        let account = store.upsert_account(&user_id, &base_url).await.unwrap();
        store
            .set_account_oauth_session(
                account.account_id,
                "OAUTHDEVICE",
                "access-1",
                "refresh-1",
                "dynamic-client",
                "store-key",
            )
            .await
            .unwrap();
        let refreshing = Client::builder()
            .homeserver_url(&base_url)
            .handle_refresh_tokens()
            .build()
            .await
            .unwrap();
        refreshing
            .oauth()
            .restore_session(
                OAuthSession {
                    client_id: ClientId::new("dynamic-client".to_owned()),
                    user: matrix_sdk::authentication::oauth::UserSession {
                        meta: matrix_sdk::SessionMeta {
                            user_id: user_id.as_str().try_into().unwrap(),
                            device_id: "OAUTHDEVICE".into(),
                        },
                        tokens: matrix_sdk::SessionTokens {
                            access_token: "access-1".to_owned(),
                            refresh_token: Some("refresh-1".to_owned()),
                        },
                    },
                },
                matrix_sdk::store::RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        let changes = refreshing.subscribe_to_session_changes();
        let cancel = CancellationToken::new();
        let watcher = tokio::spawn(watch_session_changes(
            refreshing.clone(),
            store.clone(),
            account.account_id,
            "store-key".to_owned(),
            changes,
            cancel.clone(),
        ));
        refreshing.oauth().refresh_access_token().await.unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let rotated = match store
                    .account_session(account.account_id, "store-key")
                    .await
                    .unwrap()
                {
                    Some(axon_store::StoredAccountSession::OAuth {
                        access_token,
                        refresh_token,
                        ..
                    }) => access_token == "access-2" && refresh_token == "refresh-2",
                    _ => false,
                };
                if rotated {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("refresh snapshot persisted");
        cancel.cancel();
        watcher.await.unwrap();

        let updated_account = store
            .get_account(account.account_id)
            .await
            .unwrap()
            .unwrap();
        let stored_session = store
            .account_session(account.account_id, "store-key")
            .await
            .unwrap()
            .unwrap();
        let restarted = Client::builder()
            .homeserver_url(&base_url)
            .handle_refresh_tokens()
            .build()
            .await
            .unwrap();
        crate::client::restore(&restarted, &updated_account, stored_session)
            .await
            .unwrap();
        let restarted_session = restarted.oauth().full_session().unwrap();
        assert_eq!(restarted_session.user.tokens.access_token, "access-2");
        assert_eq!(
            restarted_session.user.tokens.refresh_token.as_deref(),
            Some("refresh-2")
        );

        // A lifecycle transition is permanent, not a transient durability
        // failure. The persister must return immediately rather than consuming
        // its multi-second retry schedule and emitting a false crash warning.
        store
            .set_account_matrix_session(
                account.account_id,
                "MATRIXDEVICE",
                "matrix-access",
                "store-key",
            )
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_millis(250),
            persist_latest_with_retry(
                &restarted,
                &store,
                account.account_id,
                "store-key",
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("stale OAuth persistence should not retry");

        // The auth-aware logout path dispatches to OAuth revocation rather than
        // Matrix `/logout`. OAuth2 correctly rejects this test server's insecure
        // revocation URL; lifecycle intentionally logs and swallows that failure
        // after local teardown.
        let revocation_error = refreshing.logout().await.unwrap_err();
        assert!(revocation_error.to_string().contains("revocation"));

        sqlx_core::query::query("DELETE FROM matrix_oauth_registrations WHERE issuer_url = $1")
            .bind(&persisted.issuer_url)
            .execute(store.pool())
            .await
            .unwrap();
        store.delete_account_row(account.account_id).await.unwrap();
        server.abort();
    }
}
