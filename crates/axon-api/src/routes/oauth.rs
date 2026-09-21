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
use crate::oauth::tokens::{self, TokenError, TokenPair};
use crate::oauth::{ClientCheck, OAuthRuntime, VerifiedIdentity};
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

/// Upstream callback fields, carried in a GET query or URL-encoded POST form.
#[derive(Deserialize, utoipa::ToSchema)]
pub struct CallbackQuery {
    /// Exactly one of code or error is required.
    pub code: Option<String>,
    /// Opaque state from an unexpired, pending server-side flow.
    pub state: String,
    pub error: Option<String>,
}

/// Decode only the method's source, never merge query and POST fields. Unknown
/// fields (including Apple's unsigned user object) are ignored, never logged.
pub struct CallbackInput(CallbackQuery);

impl<S: Send + Sync> FromRequest<S> for CallbackInput {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let query: CallbackQuery = if req.method() == axum::http::Method::GET {
            let raw = req.uri().query().unwrap_or_default();
            if raw.len() > crate::oauth::MAX_CALLBACK_BYTES {
                return Err(ApiError::payload_too_large("callback too large"));
            }
            axum::extract::Query::<CallbackQuery>::try_from_uri(req.uri())
                .map_err(|_| invalid_flow())?
                .0
        } else {
            axum::extract::Form::<CallbackQuery>::from_request(req, state)
                .await
                .map_err(|_| ApiError::bad_request("invalid callback form"))?
                .0
        };
        if query.state.is_empty()
            || query.state.len() > 256
            || query
                .code
                .as_ref()
                .is_some_and(|code| code.is_empty() || code.len() > 8192)
            || query
                .error
                .as_ref()
                .is_some_and(|error| error.is_empty() || error.len() > 256)
            || query.code.is_some() == query.error.is_some()
        {
            return Err(invalid_flow());
        }
        Ok(Self(query))
    }
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
    // Logged as well as answered. This response is shown to the user rather
    // than redirected (RFC 6749 § 4.1.2.1), so it reaches a browser and not an
    // operator — and before this, the server's only record of a refused
    // sign-in was TraceLayer's access line, which carries method, path and
    // status and no reason at all. The requested `redirect_uri` is what an
    // operator has to compare against `[[oauth.clients]]`, so it goes in the
    // log; it stays out of the response body, where it would only reflect the
    // caller's own input back at them.
    match runtime.check_client(&q.client_id, &q.redirect_uri) {
        ClientCheck::Allowed => {}
        ClientCheck::UnknownClient => {
            tracing::warn!(
                client_id = %q.client_id,
                redirect_uri = %q.redirect_uri,
                "oauth authorize refused: no client is registered under this client_id"
            );
            return Err(ApiError::bad_request("unknown client_id"));
        }
        ClientCheck::RedirectUriNotRegistered => {
            tracing::warn!(
                client_id = %q.client_id,
                redirect_uri = %q.redirect_uri,
                "oauth authorize refused: redirect_uri is not registered for this client_id"
            );
            return Err(ApiError::bad_request(
                "redirect_uri is not registered for this client_id",
            ));
        }
    }
    let Some(provider) = runtime.provider(&q.provider) else {
        return Err(ApiError::bad_request("unknown or disabled provider"));
    };

    let upstream_state = tokens::generate_opaque_value();
    let upstream_nonce = tokens::generate_opaque_value();
    let expires_at = Utc::now() + AUTHORIZATION_REQUEST_TTL;
    let callback_uri = runtime.callback_url(&q.provider);

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

/// Query callback adapter; both methods share the same validation and completion.
#[utoipa::path(
    get,
    path = "/v1/oauth/{provider}/callback",
    params(
        ("provider" = String, Path, description = "Enabled upstream identity provider"),
        ("state" = String, Query, description = "Server-issued upstream state"),
        ("code" = Option<String>, Query, description = "Authorization code; mutually exclusive with error"),
        ("error" = Option<String>, Query, description = "Provider error; mutually exclusive with code"),
    ),
    responses(
        (status = 200, description = "Binding/bootstrap result or native-app handoff page", content_type = "text/html", body = String),
        (status = 303, description = "Return to the validated client with a code or sanitized OAuth error"),
        (status = 400, description = "Invalid, stale, or malformed callback", content_type = "text/html", body = String),
        (status = 403, description = "Bootstrap peer is not permitted", content_type = "text/html", body = String),
        (status = 404, description = "OAuth or provider disabled", content_type = "text/html", body = String),
        (status = 413, description = "Callback exceeds 16 KiB", content_type = "text/html", body = String),
        (status = 429, description = "Rate limit exceeded", content_type = "text/html", body = String),
    ),
    tag = "oauth", security(),
)]
pub async fn callback_get(
    store: State<Store>,
    runtime: State<Option<Arc<OAuthRuntime>>>,
    bootstrap: State<Option<BootstrapConfig>>,
    provider: Path<String>,
    peer: ConnectInfo<SocketAddr>,
    input: CallbackInput,
) -> Result<Response, ApiError> {
    callback(store, runtime, bootstrap, provider, peer, input).await
}

/// Complete either GET or bounded form POST through the same validated flow.
#[utoipa::path(
    post,
    path = "/v1/oauth/{provider}/callback",
    params(("provider" = String, Path, description = "Enabled upstream identity provider")),
    request_body(content = CallbackQuery, content_type = "application/x-www-form-urlencoded"),
    responses(
        (status = 200, description = "Binding/bootstrap result or native-app handoff page", content_type = "text/html", body = String),
        (status = 303, description = "Return to the validated client with a code or sanitized OAuth error"),
        (status = 400, description = "Invalid, stale, or malformed callback", content_type = "text/html", body = String),
        (status = 403, description = "Bootstrap peer is not permitted", content_type = "text/html", body = String),
        (status = 404, description = "OAuth or provider disabled", content_type = "text/html", body = String),
        (status = 413, description = "Callback exceeds 16 KiB", content_type = "text/html", body = String),
        (status = 429, description = "Rate limit exceeded", content_type = "text/html", body = String),
    ),
    tag = "oauth", security(),
)]
pub async fn callback(
    State(store): State<Store>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    State(bootstrap_config): State<Option<BootstrapConfig>>,
    Path(provider_name): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    CallbackInput(q): CallbackInput,
) -> Result<Response, ApiError> {
    let runtime = runtime.ok_or_else(|| ApiError::not_found("oauth is disabled"))?;
    let provider = runtime
        .provider(&provider_name)
        .ok_or_else(|| ApiError::not_found("unknown or disabled provider"))?;
    let flow = if let Some(code) = q.state.strip_prefix(BIND_STATE_PREFIX) {
        let id = Uuid::parse_str(code).map_err(|_| invalid_flow())?;
        let mut request = store
            .find_bind_request(id)
            .await?
            .ok_or_else(invalid_flow)?;
        if request.status != "pending"
            || request.provider != provider_name
            || request.expires_at <= Utc::now()
        {
            return Err(invalid_flow());
        }
        let nonce = request
            .upstream_nonce
            .take()
            .filter(|nonce| !nonce.is_empty())
            .ok_or_else(invalid_flow)?;
        CallbackFlow::Bind(request, nonce)
    } else {
        let request = store
            .find_authorization_request_by_upstream_state(&provider_name, &q.state)
            .await?
            .ok_or_else(invalid_flow)?;
        if q.state.starts_with(BOOTSTRAP_STATE_PREFIX) {
            let config = bootstrap::validate_oauth_callback(bootstrap_config, peer, &request)?;
            if !store.first_credential_bootstrap_available().await? {
                return Err(invalid_flow());
            }
            CallbackFlow::Bootstrap(request, config)
        } else {
            // Bootstrap's reserved destination can never be used as an ordinary login.
            if request.client_id == bootstrap::BOOTSTRAP_CLIENT_ID
                || !matches!(
                    runtime.check_client(&request.client_id, &request.redirect_uri),
                    ClientCheck::Allowed
                )
            {
                return Err(invalid_flow());
            }
            CallbackFlow::Login(request)
        }
    };
    if let Some(error) = q.error {
        let failure = match error.as_str() {
            "access_denied" => SignInFailure::ProviderDenied,
            "user_cancelled_authorize" if provider_name == "apple" => SignInFailure::ProviderDenied,
            "server_error" | "temporarily_unavailable" => SignInFailure::ProviderUnavailable,
            _ => SignInFailure::ProviderRejected,
        };
        return fail_callback(&store, &flow, failure).await;
    }
    let code = q.code.ok_or_else(invalid_flow)?;
    let callback_uri = runtime.callback_url(&provider_name);
    let verified = async {
        let upstream = provider.exchange_code(&code, &callback_uri).await?;
        provider
            .verify_identity_token(&upstream.id_token, Some(flow.nonce()))
            .await
    }
    .await;
    let verified = match verified {
        Ok(verified) => verified,
        Err(error) => {
            tracing::warn!(provider = %provider_name, reason = error.diagnostic_reason(), "OAuth callback verification failed");
            let failure = SignInFailure::from_oidc(&error);
            return fail_callback(&store, &flow, failure).await;
        }
    };
    match flow {
        CallbackFlow::Bind(request, _) => complete_bind(&store, verified, request).await,
        CallbackFlow::Bootstrap(request, config) => {
            bootstrap::complete_oauth_callback(&store, &runtime, config, &request, verified).await
        }
        CallbackFlow::Login(request) => {
            let Some(identity) = store
                .find_identity(&provider_name, &verified.subject)
                .await?
            else {
                return fail_callback(
                    &store,
                    &CallbackFlow::Login(request),
                    SignInFailure::IdentityNotBound,
                )
                .await;
            };
            let axon_code = tokens::generate_opaque_value();
            if !store
                .complete_authorization(request.id, identity.id, &tokens::hash_secret(&axon_code))
                .await?
            {
                return Err(invalid_flow());
            }
            let mut url =
                url::Url::parse(&request.redirect_uri).map_err(|_| ApiError::internal())?;
            url.query_pairs_mut().append_pair("code", &axon_code);
            if let Some(state) = &request.client_state {
                url.query_pairs_mut().append_pair("state", state);
            }
            Ok(deliver_authorization_code(&url))
        }
    }
}

fn invalid_flow() -> ApiError {
    tracing::warn!(reason = "invalid_or_stale_flow", "OAuth callback rejected");
    ApiError::bad_request("unknown, completed, or expired authorization flow; start sign-in again")
}

#[derive(Clone, Copy)]
enum SignInFailure {
    ProviderDenied,
    ProviderRejected,
    ProviderUnavailable,
    VerificationFailed,
    IdentityNotBound,
}

impl SignInFailure {
    fn from_oidc(error: &crate::oauth::OidcError) -> Self {
        match error {
            crate::oauth::OidcError::Http(_) | crate::oauth::OidcError::Malformed(_) => {
                Self::ProviderUnavailable
            }
            _ => Self::VerificationFailed,
        }
    }

    fn log(self, provider: &str, flow_type: &'static str) {
        tracing::warn!(
            provider,
            flow_type,
            reason = self.reason(),
            "OAuth sign-in failed"
        );
    }

    fn reason(self) -> &'static str {
        match self {
            Self::ProviderDenied => "provider_denied",
            Self::ProviderRejected => "provider_rejected",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::VerificationFailed => "verification_failed",
            Self::IdentityNotBound => "identity_not_bound",
        }
    }

    fn oauth_error(self) -> &'static str {
        match self {
            Self::ProviderDenied | Self::IdentityNotBound => "access_denied",
            // Preserve the existing wire error; description supplies the action.
            Self::ProviderUnavailable | Self::ProviderRejected | Self::VerificationFailed => {
                "temporarily_unavailable"
            }
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::ProviderDenied => "Sign-in was canceled or denied by the provider. Start sign-in again when you are ready.",
            Self::ProviderRejected => "The provider did not complete sign-in. Start a new sign-in attempt.",
            Self::ProviderUnavailable => "Axon could not complete sign-in with the provider. Start a new sign-in attempt later. If this continues, contact the instance owner.",
            Self::VerificationFailed => "Axon could not verify the sign-in credential. Start sign-in again. If this continues, contact the instance owner.",
            Self::IdentityNotBound => "This account is not authorized to sign in to this Axon instance. Ask the instance owner to bind it, or use another account.",
        }
    }
}

enum CallbackFlow {
    Login(axon_store::AuthorizationRequest),
    Bind(BindRequest, String),
    Bootstrap(axon_store::AuthorizationRequest, BootstrapConfig),
}

impl CallbackFlow {
    fn nonce(&self) -> &str {
        match self {
            Self::Login(request) | Self::Bootstrap(request, _) => &request.upstream_nonce,
            Self::Bind(_, nonce) => nonce,
        }
    }
}

/// Invalidate only a still-pending flow; a concurrent success must never be
/// overwritten by an error callback. Starting a new flow is the recovery path.
async fn fail_callback(
    store: &Store,
    flow: &CallbackFlow,
    failure: SignInFailure,
) -> Result<Response, ApiError> {
    let (provider, flow_type) = match flow {
        CallbackFlow::Login(request) => (request.provider.as_str(), "login"),
        CallbackFlow::Bind(request, _) => (request.provider.as_str(), "bind"),
        CallbackFlow::Bootstrap(request, _) => (request.provider.as_str(), "bootstrap"),
    };
    failure.log(provider, flow_type);
    let claimed = match flow {
        CallbackFlow::Login(request) | CallbackFlow::Bootstrap(request, _) => {
            store.cancel_authorization(request.id).await?
        }
        CallbackFlow::Bind(request, _) => store.cancel_bind_request(request.device_code).await?,
    };
    if !claimed {
        return Err(invalid_flow());
    }
    match flow {
        CallbackFlow::Login(request) => {
            let mut url =
                url::Url::parse(&request.redirect_uri).map_err(|_| ApiError::internal())?;
            url.query_pairs_mut()
                .append_pair("error", failure.oauth_error())
                .append_pair("error_description", failure.description());
            if let Some(state) = &request.client_state {
                url.query_pairs_mut().append_pair("state", state);
            }
            if matches!(url.scheme(), "http" | "https") {
                Ok(Redirect::to(url.as_str()).into_response())
            } else {
                Ok(Html(handoff_page_with_status(url.as_str(), false)).into_response())
            }
        }
        CallbackFlow::Bind(_, _) | CallbackFlow::Bootstrap(_, _) => {
            let retry = if matches!(flow, CallbackFlow::Bind(_, _)) {
                "Run axon oauth bind again to retry."
            } else {
                "Reopen your original setup URL to retry."
            };
            // Both interpolated strings are application-owned constants.
            Ok(Html(format!(
                "<!doctype html><title>Sign-in did not complete</title><p>{}</p><p>{retry}</p>",
                failure.description()
            ))
            .into_response())
        }
    }
}

/// Callback pages may contain a one-time credential or handoff target. Never
/// cache them or send their URL as a referrer; failures never reflect input.
pub async fn callback_response(mut response: Response) -> Response {
    if response.status().is_client_error() || response.status().is_server_error() {
        response = (response.status(), Html("<!doctype html><title>Sign-in did not complete</title><p>Sign-in did not complete. Return to Axon or reopen your setup URL to start a new attempt.</p>")).into_response();
    }
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        axum::http::header::REFERRER_POLICY,
        axum::http::HeaderValue::from_static("no-referrer"),
    );
    response
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
    handoff_page_with_status(target, true)
}

fn handoff_page_with_status(target: &str, success: bool) -> String {
    let title = if success {
        "Signed in"
    } else {
        "Sign-in did not complete"
    };
    let message = if success {
        "Returning you to Axon."
    } else {
        "Return to Axon to try again."
    };
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>{title}</title>
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
    <h1>{title}</h1>
    <p>{message} You can close this tab.</p>
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
        _ => {
            tracing::warn!(
                flow_type = "token",
                reason = "unsupported_grant_type",
                "OAuth token request rejected"
            );
            return oauth_error_response(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "Unsupported token grant type.",
            );
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
/// by its human-typeable `user_code`, reads the CLI-created nonce, and redirects
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

    // Created by the CLI before the URL was printed; GET/HEAD never mutate it.
    // Legacy pending rows without a nonce must restart with the updated CLI.
    let nonce = request
        .upstream_nonce
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("bind has no nonce; rerun oauth bind"))?;

    let callback_uri = runtime.callback_url(&request.provider);
    let state = format!("{BIND_STATE_PREFIX}{}", request.device_code);
    let redirect_url = provider.authorize_url(&state, nonce, &callback_uri);
    Ok(Redirect::to(&redirect_url).into_response())
}

/// Finish an `axon oauth bind` handshake after shared verification: bind the
/// asserted identity (UPSERT — this is specifically how a *new* identity
/// gets bound, unlike Path A's [`callback`] which requires one already
/// exists), and mark the bind request complete. Returns a small static page
/// rather than a redirect: unlike Path A, there's no client `redirect_uri`
/// to bounce an ad hoc admin browser tab back to.
async fn complete_bind(
    store: &Store,
    verified: VerifiedIdentity,
    bind_request: BindRequest,
) -> Result<Response, ApiError> {
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
            Err(_) => {
                tracing::warn!(
                    flow_type = "token",
                    reason = "malformed_request",
                    "OAuth token request rejected"
                );
                Err(oauth_error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Invalid token request. Use a URL-encoded form with the required fields.",
                ))
            }
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
        TokenError::InvalidGrant(reason) => {
            tracing::warn!(flow_type = "token", reason, "OAuth token request rejected");
            oauth_error_response(StatusCode::BAD_REQUEST, "invalid_grant", "The sign-in code or token is invalid, expired, already used, or does not match this request. Start sign-in again.")
        }
        TokenError::UnknownProvider => {
            tracing::warn!(
                flow_type = "token",
                reason = "unknown_provider",
                "OAuth token request rejected"
            );
            oauth_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "unknown or disabled provider",
            )
        }
        TokenError::NotBound => {
            tracing::warn!(
                flow_type = "token",
                reason = "identity_not_bound",
                "OAuth token request rejected"
            );
            oauth_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                SignInFailure::IdentityNotBound.description(),
            )
        }
        TokenError::Oidc(err) => {
            tracing::warn!(
                flow_type = "token",
                reason = err.diagnostic_reason(),
                "OAuth token verification failed"
            );
            oauth_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                SignInFailure::from_oidc(&err).description(),
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
mod failure_tests {
    use super::*;
    use crate::oauth::OidcError;
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl Write for LogBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn failure_diagnostics_are_actionable_and_do_not_log_upstream_values() {
        let buffer = LogBuffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let responses = tracing::subscriber::with_default(subscriber, || {
            for failure in [
                SignInFailure::IdentityNotBound,
                SignInFailure::ProviderDenied,
                SignInFailure::ProviderUnavailable,
                SignInFailure::VerificationFailed,
            ] {
                failure.log("apple", "login");
            }
            let mut responses = Vec::new();
            for error in [
                OidcError::Http("PRIVATE_SENTINEL".into()),
                OidcError::Malformed("PRIVATE_SENTINEL".into()),
                OidcError::InvalidIssuer("PRIVATE_SENTINEL".into()),
                OidcError::InvalidAudience("PRIVATE_SENTINEL".into()),
                OidcError::InvalidNonce,
                OidcError::Expired("PRIVATE_SENTINEL".into()),
                OidcError::BadSignature("PRIVATE_SENTINEL".into()),
                OidcError::DisallowedAlgorithm("PRIVATE_SENTINEL".into()),
            ] {
                responses.push(token_error_into_response(TokenError::Oidc(error)));
            }
            responses.push(token_error_into_response(TokenError::NotBound));
            responses.push(token_error_into_response(TokenError::InvalidGrant(
                "unknown refresh token",
            )));
            responses.push(crate::auth::invalid_token_response());
            responses
        });
        for response in responses {
            assert!(response.status().is_client_error());
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body = std::str::from_utf8(&body).unwrap();
            assert!(!body.contains("PRIVATE_SENTINEL"));
            assert!(
                body.contains("sign-in")
                    || body.contains("Sign in again")
                    || body.contains("bind it")
            );
        }
        let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(!logs.contains("PRIVATE_SENTINEL"));
        for reason in [
            "identity_not_bound",
            "provider_denied",
            "provider_unavailable",
            "verification_failed",
            "invalid_bearer_token",
            "signature_invalid",
            "token_time_invalid",
            "nonce_mismatch",
            "unknown refresh token",
            "issuer_mismatch",
            "audience_mismatch",
            "upstream_request_failed",
            "upstream_response_invalid",
            "algorithm_disallowed",
        ] {
            assert!(
                logs.contains(reason),
                "missing diagnostic category {reason}"
            );
        }
        assert!(logs.contains("provider=\"apple\""));
        assert!(logs.contains("flow_type=\"login\""));
    }
}

#[cfg(test)]
mod providers_tests {
    use super::*;
    use crate::oauth::provider::{OidcError, UpstreamTokens, VerifiedIdentity};
    use crate::oauth::OidcProvider;
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
