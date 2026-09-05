//! `/v1/oauth/*` handlers (M14b, ADR 0054): axon as its own OAuth 2.0
//! Authorization Server, and an OIDC Relying Party to upstream providers.
//!
//! Deliberately **un-gated** — not behind `require_bearer` — since this is
//! how a client obtains a bearer token in the first place. Every handler
//! checks `oauth.enabled` (and, where relevant, the named provider's
//! `enabled`) first and returns `404` when off, exactly like
//! [`routes::search`](crate::routes::search) does for `search.enabled` — but
//! `404`, not `503`: an unauthenticated caller of a *disabled* surface should
//! see "this route doesn't exist" the same as a genuinely unregistered path,
//! not "come back later" (`/v1/ws` is the existing router's proof that a
//! specific route already beats the authed sub-router's catch-all regardless
//! of merge order; this router adds a third, equally-specific sibling).
//!
//! `POST /v1/oauth/token`'s body is plain RFC 6749 JSON — `{"access_token",
//! "token_type", "expires_in", "refresh_token"}` on success,
//! `{"error", "error_description"}` on failure — not this crate's
//! `ApiResponse`/`ApiError` envelope; see [`TokenSuccessBody`]/[`OAuthErrorBody`].

use std::net::SocketAddr;
use std::sync::Arc;

use axon_store::{BindRequest, NewAuthorizationRequest, Store};
use axum::extract::{ConnectInfo, FromRequest, Request, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Json;
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::extract::{Path, Query};
use crate::oauth::provider::OidcError;
use crate::oauth::tokens::{self, TokenError, TokenPair};
use crate::oauth::{OAuthRuntime, OidcProvider};
use crate::response::{ApiError, ApiResponse};
use crate::routes::bootstrap::{self, BOOTSTRAP_STATE_PREFIX};
use crate::state::BootstrapConfig;

/// How long a Path A flow (and its axon-minted code) stays redeemable.
const AUTHORIZATION_REQUEST_TTL: ChronoDuration = ChronoDuration::minutes(10);

/// Tags a bind handshake's outgoing `state` so [`callback`] can tell it apart
/// from a Path A `state` explicitly, rather than by relying on the two
/// generators' outputs happening never to overlap in shape.
const BIND_STATE_PREFIX: &str = "bind:";

/// `GET /v1/oauth/authorize` query parameters (RFC 6749 §4.1.1, PKCE's
/// `code_challenge`/`code_challenge_method` per RFC 7636, plus `provider` to
/// pick which upstream IdP this flow uses).
#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub response_type: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub provider: String,
    /// The client's own CSRF-binding state, if it sent one. Opaque to axon;
    /// echoed back verbatim on the final redirect.
    pub state: Option<String>,
}

/// `GET /v1/oauth/{provider}/callback` query parameters — what the upstream
/// provider's own authorization server appends to its redirect back to axon.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

/// One sign-in provider this instance has enabled.
///
/// An object rather than a bare string so a later addition — a display label,
/// an icon, whether the provider supports Path B — is an additive field rather
/// than a reshaped array, which ADR 0099's additive-first policy would
/// otherwise make a breaking change.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct OAuthProviderDto {
    /// The value to pass as `provider` to `GET /v1/oauth/authorize`, e.g.
    /// `"google"`.
    pub provider: String,
}

/// `GET /v1/oauth/providers`: which sign-in providers this instance offers.
///
/// A client cannot know this by configuration. Which providers exist is a
/// property of the *server*, and a client that is distributed as a binary —
/// the desktop and mobile shells of ADR 0102 — is pointed at a server it has
/// never seen and cannot have been built against. Baking the list in at build
/// time, as the web client's `VITE_AXON_OAUTH_PROVIDERS` does, works only when
/// whoever builds the bundle also runs the server.
///
/// Deliberately un-gated, like the rest of this module: it is read *before*
/// there is a token, to decide which buttons the sign-in screen has. It
/// therefore reveals which IdPs an instance trusts to anyone who can reach it,
/// which is the same thing its sign-in page would show them, and no more than
/// `/v1/oauth/authorize` already tells them by accepting or rejecting a
/// `provider`.
///
/// `404` when OAuth is off, matching this module's stated policy: an
/// unauthenticated caller of a disabled surface sees "no such route", not
/// "come back later".
#[utoipa::path(
    get,
    path = "/v1/oauth/providers",
    responses(
        (status = 200, description = "The enabled sign-in providers", body = ApiResponse<Vec<OAuthProviderDto>>),
        (status = 404, description = "OAuth is disabled", body = crate::response::ErrorResponse),
    ),
    tag = "oauth",
    security(),
)]
pub async fn providers(
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
) -> Result<ApiResponse<Vec<OAuthProviderDto>>, ApiError> {
    let Some(runtime) = runtime else {
        return Err(ApiError::not_found("oauth is disabled"));
    };
    // Sorted so the sign-in screen's button order is stable across restarts;
    // `providers` is a HashMap, whose iteration order is not.
    let mut names = runtime.providers.keys().copied().collect::<Vec<_>>();
    names.sort_unstable();
    Ok(ApiResponse::new(
        names
            .into_iter()
            .map(|provider| OAuthProviderDto {
                provider: provider.to_owned(),
            })
            .collect(),
    ))
}

/// Start Path A: validate the request, record it, and redirect the browser
/// to the named upstream provider. An invalid `client_id`/`redirect_uri`
/// pair is rejected directly (`400`), never redirected — redirecting to an
/// unregistered URI is exactly what the exact-match allow-list exists to
/// prevent.
pub async fn authorize(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    Query(q): Query<AuthorizeQuery>,
) -> Result<Response, ApiError> {
    let Some(runtime) = runtime else {
        return Err(ApiError::not_found("oauth is disabled"));
    };

    if q.response_type != "code" {
        return Err(ApiError::bad_request("response_type must be \"code\""));
    }
    if q.code_challenge_method != "S256" {
        return Err(ApiError::bad_request(
            "code_challenge_method must be \"S256\"",
        ));
    }
    if !runtime.redirect_uri_allowed(&q.client_id, &q.redirect_uri) {
        return Err(ApiError::bad_request("unknown client_id or redirect_uri"));
    }
    let Some(provider) = runtime.provider(&q.provider) else {
        return Err(ApiError::bad_request("unknown or disabled provider"));
    };

    let upstream_state = tokens::generate_opaque_value();
    let upstream_nonce = tokens::generate_opaque_value();
    let expires_at = Utc::now() + AUTHORIZATION_REQUEST_TTL;
    let callback_uri = callback_url(&runtime, &q.provider);

    store
        .create_authorization_request(&NewAuthorizationRequest {
            client_id: &q.client_id,
            redirect_uri: &q.redirect_uri,
            code_challenge: &q.code_challenge,
            code_challenge_method: &q.code_challenge_method,
            client_state: q.state.as_deref(),
            provider: &q.provider,
            upstream_state: &upstream_state,
            upstream_nonce: &upstream_nonce,
            expires_at,
        })
        .await?;

    let redirect_url = provider.authorize_url(&upstream_state, &upstream_nonce, &callback_uri);
    Ok(Redirect::to(&redirect_url).into_response())
}

/// Finish Path A's upstream leg, **or** finish an `axon oauth bind`
/// handshake — both land here, since both are "the upstream provider
/// redirected back to axon" and share a `(provider, state)` pair to look a
/// pending flow up by. Disambiguated by an explicit [`BIND_STATE_PREFIX`] tag
/// on the `state` value, not by its incidental shape — Path A's `state`
/// ([`tokens::generate_opaque_value`]) happens to never be UUID-parseable
/// today, but relying on that would couple this dispatch to a format neither
/// generator promises to keep, and the tag costs nothing extra to check.
/// Path A only ever authenticates an **already-bound** owner; a bind
/// handshake is the opposite — it's specifically how a *new* identity gets
/// bound.
pub async fn callback(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    State(bootstrap_config): State<Option<BootstrapConfig>>,
    Path(provider_name): Path<String>,
    Query(q): Query<CallbackQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    let Some(runtime) = runtime else {
        return Err(ApiError::not_found("oauth is disabled"));
    };
    let Some(provider) = runtime.provider(&provider_name) else {
        return Err(ApiError::not_found("unknown or disabled provider"));
    };

    if q.state.starts_with(BOOTSTRAP_STATE_PREFIX) {
        return bootstrap::complete_oauth_callback(bootstrap::BootstrapOauthCallback {
            store: &store,
            bootstrap: bootstrap_config,
            runtime: &runtime,
            provider_name: &provider_name,
            provider,
            code: &q.code,
            state: &q.state,
            peer,
        })
        .await;
    }

    if let Some(device_code) = q
        .state
        .strip_prefix(BIND_STATE_PREFIX)
        .and_then(|rest| Uuid::parse_str(rest).ok())
    {
        if let Some(bind_request) = store.find_bind_request(device_code).await? {
            // `expires_at` is checked here too, not just inside
            // `complete_bind_request`'s atomic claim — an already-expired
            // row is still `status = 'pending'` until the next unrelated
            // `create_bind_request` sweeps it, and there's no reason to
            // burn a one-time upstream authorization code and do a full
            // token-exchange/verification round trip on a flow that's
            // already dead; `complete_bind_request` remains the actual
            // security-relevant guard against the tighter race (expiry
            // landing between this check and that claim).
            if bind_request.status == "pending"
                && bind_request.provider == provider_name
                && bind_request.expires_at > Utc::now()
            {
                let callback_uri = callback_url(&runtime, &provider_name);
                return complete_bind(&store, provider, &q.code, &callback_uri, bind_request).await;
            }
        }
    }

    let request = store
        .find_authorization_request_by_upstream_state(&provider_name, &q.state)
        .await?
        .ok_or_else(|| ApiError::bad_request("unknown or expired authorization flow"))?;

    let callback_uri = callback_url(&runtime, &provider_name);
    let upstream = provider
        .exchange_code(&q.code, &callback_uri)
        .await
        .map_err(oidc_error_to_api_error)?;
    let verified = provider
        .verify_identity_token(&upstream.id_token, Some(&request.upstream_nonce))
        .await
        .map_err(oidc_error_to_api_error)?;

    let identity = store
        .find_identity(&provider_name, &verified.subject)
        .await?
        .ok_or_else(|| ApiError::forbidden("this identity is not bound to this axon instance"))?;

    let axon_code = tokens::generate_opaque_value();
    if !store
        .complete_authorization(request.id, identity.id, &tokens::hash_secret(&axon_code))
        .await?
    {
        return Err(ApiError::conflict(
            "authorization flow already completed or expired",
        ));
    }

    let mut redirect_url =
        url::Url::parse(&request.redirect_uri).map_err(|_| ApiError::internal())?;
    redirect_url
        .query_pairs_mut()
        .append_pair("code", &axon_code);
    if let Some(client_state) = &request.client_state {
        redirect_url
            .query_pairs_mut()
            .append_pair("state", client_state);
    }
    Ok(deliver_authorization_code(&redirect_url))
}

/// Hand the authorization code back to the client that asked for it.
///
/// A browser-hosted client gets the redirect it always got. A client whose
/// `redirect_uri` is a private scheme — a desktop or mobile app, RFC 8252 § 7.1
/// — gets a page instead, for a reason that only shows up on a real desktop:
/// a bare `302` to `myapp://…` hands the URL to the OS and leaves the tab with
/// no document to render, so it spins forever on a sign-in that has in fact
/// already succeeded. Reported on Windows against Edge; the tab had to be
/// closed by hand every time.
///
/// The page performs the same hand-off from script, and says the tab can be
/// closed. It also degrades: if the scheme has no handler, or script is off,
/// the link is still there to click, where the redirect would simply have
/// failed with a console message the user never sees.
fn deliver_authorization_code(redirect_url: &url::Url) -> Response {
    if matches!(redirect_url.scheme(), "http" | "https") {
        return Redirect::to(redirect_url.as_str()).into_response();
    }
    Html(handoff_page(redirect_url.as_str())).into_response()
}

/// The interstitial for a private-scheme client.
///
/// The script reads the target back out of the DOM rather than having it
/// interpolated into it. `client_state` is opaque data this server echoes on
/// behalf of the client, so it is attacker-influenceable in principle: it
/// arrives here percent-encoded by `query_pairs_mut`, is escaped again for the
/// attribute, and never enters a script context at all. `getAttribute` rather
/// than `.href` so the browser's URL normalisation cannot alter a non-special
/// scheme on the way through.
fn handoff_page(target: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Signed in</title>
    <style>
      body {{
        font-family: system-ui, sans-serif;
        margin: 4rem auto;
        max-width: 28rem;
        padding: 0 1rem;
        line-height: 1.5;
      }}
    </style>
  </head>
  <body>
    <h1>Signed in</h1>
    <p>Returning you to Axon. You can close this tab.</p>
    <p><a id="handoff" href="{target}">Open Axon</a></p>
    <script>
      var link = document.getElementById('handoff')
      if (link) {{
        window.location.replace(link.getAttribute('href'))
      }}
    </script>
  </body>
</html>
"#,
        target = bootstrap::html_escape(target)
    )
}

/// `POST /v1/oauth/token` (RFC 6749 §4.1.3, §6, and Path B's custom
/// `urn:axon:identity_token` grant): redeem an authorization code, a refresh
/// token, or a native identity token for an axon access/refresh pair.
pub async fn token(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    OAuthForm(body): OAuthForm<TokenRequest>,
) -> Response {
    let Some(runtime) = runtime else {
        return ApiError::not_found("oauth is disabled").into_response();
    };

    let result = match body.grant_type.as_str() {
        "authorization_code" => {
            match (
                body.code.as_deref(),
                body.code_verifier.as_deref(),
                body.client_id.as_deref(),
                body.redirect_uri.as_deref(),
            ) {
                (Some(code), Some(verifier), Some(client_id), Some(redirect_uri)) => {
                    tokens::redeem_authorization_code(
                        &store, &runtime, code, verifier, client_id, redirect_uri,
                    )
                    .await
                }
                _ => Err(TokenError::InvalidGrant(
                    "authorization_code grant requires code, code_verifier, client_id, redirect_uri",
                )),
            }
        }
        "refresh_token" => match body.refresh_token.as_deref() {
            Some(refresh_token) => {
                tokens::redeem_refresh_token(&store, &runtime, refresh_token).await
            }
            None => Err(TokenError::InvalidGrant(
                "refresh_token grant requires refresh_token",
            )),
        },
        "urn:axon:identity_token" => {
            match (
                body.provider.as_deref(),
                body.identity_token.as_deref(),
                body.client_id.as_deref(),
            ) {
                (Some(provider), Some(identity_token), Some(client_id)) => {
                    tokens::redeem_identity_token(
                        &store,
                        &runtime,
                        provider,
                        identity_token,
                        client_id,
                    )
                    .await
                }
                _ => Err(TokenError::InvalidGrant(
                    "urn:axon:identity_token grant requires provider, identity_token, client_id",
                )),
            }
        }
        other => {
            return oauth_error_response(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                format!("unsupported grant_type {other:?}"),
            )
        }
    };

    match result {
        Ok(pair) => token_success_response(pair),
        Err(err) => token_error_into_response(err),
    }
}

/// `GET /v1/oauth/bind` query parameters — the `user_code` the CLI printed.
#[derive(Debug, Deserialize)]
pub struct BindQuery {
    pub user_code: String,
}

/// `GET /v1/oauth/bind?user_code=...` — the CLI device-code handshake's
/// browser leg (`axon oauth bind`, ADR 0054). Looks up the pending request
/// by its human-typeable `user_code`, stashes a fresh nonce, and redirects
/// straight to the upstream provider — the bind request's own `device_code`
/// doubles as the `state` sent upstream (see [`callback`]).
pub async fn bind(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    Query(q): Query<BindQuery>,
) -> Result<Response, ApiError> {
    let Some(runtime) = runtime else {
        return Err(ApiError::not_found("oauth is disabled"));
    };
    let request = store
        .find_bind_request_by_user_code(&q.user_code)
        .await?
        .ok_or_else(|| ApiError::bad_request("unknown or expired bind code"))?;
    let Some(provider) = runtime.provider(&request.provider) else {
        return Err(ApiError::not_found("unknown or disabled provider"));
    };

    let nonce = tokens::generate_opaque_value();
    if !store
        .set_bind_request_upstream_nonce(request.device_code, &nonce)
        .await?
    {
        // Lost a race against expiry or another completion between the
        // lookup above and this write — redirecting upstream now would send
        // the browser off with a nonce that was never actually persisted,
        // which `complete_bind` would later read back as `None` and (since
        // `verify_identity_token` treats a missing expected nonce as
        // "nothing to check") silently skip nonce verification entirely.
        return Err(ApiError::bad_request("unknown or expired bind code"));
    }

    let callback_uri = callback_url(&runtime, &request.provider);
    let state = format!("{BIND_STATE_PREFIX}{}", request.device_code);
    let redirect_url = provider.authorize_url(&state, &nonce, &callback_uri);
    Ok(Redirect::to(&redirect_url).into_response())
}

/// Finish an `axon oauth bind` handshake: exchange the provider's code,
/// verify the id_token against the nonce [`bind`] stashed, bind the
/// asserted identity (UPSERT — this is specifically how a *new* identity
/// gets bound, unlike Path A's [`callback`] which requires one already
/// exists), and mark the bind request complete. Returns a small static page
/// rather than a redirect: unlike Path A, there's no client `redirect_uri`
/// to bounce an ad hoc admin browser tab back to.
async fn complete_bind(
    store: &Store,
    provider: &Arc<dyn OidcProvider>,
    code: &str,
    callback_uri: &str,
    bind_request: BindRequest,
) -> Result<Response, ApiError> {
    let upstream = provider
        .exchange_code(code, callback_uri)
        .await
        .map_err(oidc_error_to_api_error)?;
    let verified = provider
        .verify_identity_token(&upstream.id_token, bind_request.upstream_nonce.as_deref())
        .await
        .map_err(oidc_error_to_api_error)?;

    // Claims the bind request and binds the identity atomically — the
    // identity is only ever written if the request was genuinely still
    // `pending`/unexpired at the instant this ran, closing the window a
    // separate "UPSERT identity, then conditionally complete" pair would
    // leave between an unconditional write and its expiry check.
    if store
        .complete_bind_request(
            bind_request.device_code,
            &bind_request.provider,
            &verified.subject,
            verified.email.as_deref(),
        )
        .await?
        .is_none()
    {
        return Err(ApiError::conflict(
            "bind request already completed or expired",
        ));
    }

    Ok(
        Html("<!doctype html><title>axon</title><p>Signed in. You can close this window.</p>")
            .into_response(),
    )
}

fn callback_url(runtime: &OAuthRuntime, provider_name: &str) -> String {
    format!(
        "{}/v1/oauth/{provider_name}/callback",
        runtime.external_base_url
    )
}

fn oidc_error_to_api_error(err: OidcError) -> ApiError {
    tracing::warn!(error = %err, "oidc verification failed");
    ApiError::bad_gateway("upstream identity verification failed")
}

/// `POST /v1/oauth/token`'s form body. Fields are `Option` because which are
/// required depends on `grant_type` — validated per-grant in [`token`], not
/// via serde, so a request naming an unrecognized grant type still gets a
/// clean `unsupported_grant_type` rather than a generic deserialization
/// error about unrelated missing fields.
#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub code_verifier: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub identity_token: Option<String>,
}

/// `application/x-www-form-urlencoded` extractor (RFC 6749 §4.1.3 mandates
/// this content type for the token endpoint, not JSON) whose rejection is an
/// RFC 6749 `invalid_request` body rather than axum's default plain text.
pub struct OAuthForm<T>(pub T);

impl<T, S> FromRequest<S> for OAuthForm<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Form::<T>::from_request(req, state).await {
            Ok(axum::extract::Form(value)) => Ok(OAuthForm(value)),
            Err(rejection) => Err(oauth_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                rejection.body_text(),
            )),
        }
    }
}

/// `POST /v1/oauth/token`'s success body (RFC 6749 §5.1) — plain, not
/// wrapped in `{"data": ...}`.
#[derive(Debug, Serialize)]
struct TokenSuccessBody {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
    refresh_token: String,
}

/// `POST /v1/oauth/token`'s error body (RFC 6749 §5.2).
#[derive(Debug, Serialize)]
struct OAuthErrorBody {
    error: &'static str,
    error_description: String,
}

fn token_success_response(pair: TokenPair) -> Response {
    Json(TokenSuccessBody {
        access_token: pair.access_token,
        token_type: "Bearer",
        expires_in: pair.expires_in,
        refresh_token: pair.refresh_token,
    })
    .into_response()
}

fn oauth_error_response(
    status: StatusCode,
    error: &'static str,
    description: impl Into<String>,
) -> Response {
    (
        status,
        Json(OAuthErrorBody {
            error,
            error_description: description.into(),
        }),
    )
        .into_response()
}

fn token_error_into_response(err: TokenError) -> Response {
    match err {
        TokenError::InvalidGrant(msg) => {
            oauth_error_response(StatusCode::BAD_REQUEST, "invalid_grant", msg)
        }
        TokenError::UnknownProvider => oauth_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "unknown or disabled provider",
        ),
        TokenError::NotBound => oauth_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "identity is not bound to this axon instance",
        ),
        TokenError::Oidc(err) => {
            tracing::warn!(error = %err, "oidc verification failed redeeming a token");
            oauth_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "identity token verification failed",
            )
        }
        TokenError::Store(err) => {
            tracing::error!(error = %err, "store error serving oauth token request");
            oauth_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal server error",
            )
        }
    }
}

#[cfg(test)]
mod providers_tests {
    use super::*;
    use crate::oauth::provider::{OidcError, UpstreamTokens, VerifiedIdentity};
    use axon_core::OauthConfig;
    use std::collections::HashMap;

    /// The trait's shape is irrelevant here — `providers` only ever reads the
    /// map's keys — so this exists purely to occupy a slot in it.
    struct StubProvider(&'static str);

    #[async_trait::async_trait]
    impl OidcProvider for StubProvider {
        fn name(&self) -> &'static str {
            self.0
        }
        fn authorize_url(&self, _state: &str, _nonce: &str, _redirect_uri: &str) -> String {
            unreachable!("providers() never starts a flow")
        }
        async fn exchange_code(
            &self,
            _code: &str,
            _redirect_uri: &str,
        ) -> Result<UpstreamTokens, OidcError> {
            unreachable!("providers() never exchanges a code")
        }
        async fn verify_identity_token(
            &self,
            _token: &str,
            _nonce: Option<&str>,
        ) -> Result<VerifiedIdentity, OidcError> {
            unreachable!("providers() never verifies a token")
        }
    }

    fn runtime(names: &[&'static str]) -> Arc<OAuthRuntime> {
        let providers = names
            .iter()
            .map(|name| {
                let provider: Arc<dyn OidcProvider> = Arc::new(StubProvider(name));
                (*name, provider)
            })
            .collect::<HashMap<_, _>>();
        Arc::new(OAuthRuntime::new(&OauthConfig::default(), providers))
    }

    #[tokio::test]
    async fn lists_only_the_enabled_providers() {
        let response = providers(State(Some(runtime(&["google"]))))
            .await
            .expect("enabled");
        let names = response
            .data
            .iter()
            .map(|p| p.provider.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["google"]);
    }

    #[tokio::test]
    async fn sorts_them_so_the_sign_in_screen_is_stable() {
        // `OAuthRuntime::providers` is a HashMap; unsorted, the buttons would
        // reorder between restarts for no reason the user can see.
        let response = providers(State(Some(runtime(&["microsoft", "apple", "google"]))))
            .await
            .expect("enabled");
        let names = response
            .data
            .iter()
            .map(|p| p.provider.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["apple", "google", "microsoft"]);
    }

    #[tokio::test]
    async fn is_404_when_oauth_is_disabled() {
        // Not 503: an unauthenticated caller of a disabled surface should see
        // "no such route", the same as a genuinely unregistered path.
        let error = providers(State(None)).await.expect_err("disabled");
        assert_eq!(error.into_response().status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn is_empty_rather_than_absent_when_none_are_configured() {
        // OAuth on with no provider wired up is a real configuration; the
        // client needs "none available", not a 404 it would read as "no such
        // server".
        let response = providers(State(Some(runtime(&[])))).await.expect("enabled");
        assert!(response.data.is_empty());
    }
}

#[cfg(test)]
mod handoff_tests {
    use super::{deliver_authorization_code, handoff_page};
    use axum::http::{header::LOCATION, StatusCode};
    use axum::response::IntoResponse;

    /// Build the delivery URL the same way `callback` does, so the encoding
    /// under test is the encoding that actually ships.
    fn delivery(redirect_uri: &str, state: &str) -> url::Url {
        let mut url = url::Url::parse(redirect_uri).expect("redirect uri");
        url.query_pairs_mut().append_pair("code", "abc123");
        url.query_pairs_mut().append_pair("state", state);
        url
    }

    #[test]
    fn a_browser_client_still_gets_a_redirect() {
        // The web client's flow is unchanged; adding an interstitial to a
        // working browser sign-in would be a regression for every deployment.
        let response =
            deliver_authorization_code(&delivery("https://axon.example.com/oauth/callback", "s1"))
                .into_response();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(LOCATION)
            .expect("Location")
            .to_str()
            .expect("ascii");
        assert!(location.starts_with("https://axon.example.com/oauth/callback?"));
        assert!(location.contains("code=abc123"));
    }

    #[test]
    fn a_private_scheme_client_gets_a_page_instead() {
        // A bare 302 to a private scheme hands the URL to the OS and leaves
        // the tab with nothing to render, so it spins on a sign-in that has
        // already succeeded.
        let response =
            deliver_authorization_code(&delivery("axon://oauth/callback", "s1")).into_response();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(LOCATION).is_none());
    }

    #[test]
    fn the_page_carries_the_target_for_script_and_for_a_click() {
        // Both paths matter: script performs the hand-off, and the link is
        // what is left when the scheme has no handler or script is off.
        let page = handoff_page(delivery("axon://oauth/callback", "s1").as_str());

        assert!(page.contains(r#"id="handoff""#));
        assert!(page.contains("axon://oauth/callback?code=abc123&amp;state=s1"));
        assert!(page.contains("getAttribute('href')"));
    }

    #[test]
    fn percent_encoding_is_the_first_line_of_defence() {
        // `state` is opaque data echoed on the client's behalf, so it is
        // attacker-influenceable in principle. Going through the URL is what
        // neutralises it: `query_pairs_mut` percent-encodes, so the dangerous
        // characters never reach the markup as themselves.
        let page = handoff_page(
            delivery("axon://oauth/callback", "\"><script>alert(1)</script>").as_str(),
        );

        assert!(!page.contains("alert(1)"));
        // The one <script> element is ours.
        assert_eq!(page.matches("<script>").count(), 1);
    }

    #[test]
    fn the_attribute_is_escaped_independently_of_that() {
        // And the escaping is a real second layer, not decoration — asserted
        // by handing `handoff_page` a raw string rather than a URL-built one.
        // Composing through `delivery` cannot test this: percent-encoding
        // removes the characters first, so the assertion passes with the
        // escaping deleted, which is exactly what it did before this split.
        let page = handoff_page("axon://oauth/callback?state=\"><script>alert(1)</script>");

        assert!(!page.contains("\"><script>"));
        assert!(page.contains("&quot;&gt;&lt;script&gt;"));
        assert_eq!(page.matches("<script>").count(), 1);
    }
}
