//! One-time web bootstrap for the first client credential.

use std::net::SocketAddr;
use std::sync::Arc;

use axon_store::{NewAuthorizationRequest, Store};
use axum::body::Body;
use axum::extract::{ConnectInfo, Form, State};
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Deserialize;

use crate::extract::Path;
use crate::oauth::provider::OidcError;
use crate::oauth::tokens::{self, TokenPair};
use crate::oauth::{OAuthRuntime, OidcProvider};
use crate::response::ApiError;
use crate::state::BootstrapConfig;

const BOOTSTRAP_TOKEN_LABEL: &str = "bootstrap-web";
/// Mirrors the setup page's `maxlength="80"` on the label field, which is
/// HTML-only and does nothing against a raw POST — this is the server-side
/// backstop.
const BOOTSTRAP_TOKEN_LABEL_MAX_LEN: usize = 80;
const BOOTSTRAP_CLIENT_ID: &str = "bootstrap-web";
const BOOTSTRAP_REDIRECT_URI: &str = "urn:axon:bootstrap";
const BOOTSTRAP_CODE_CHALLENGE: &str = "bootstrap-web";
const AUTHORIZATION_REQUEST_TTL: ChronoDuration = ChronoDuration::minutes(10);

pub(crate) const BOOTSTRAP_STATE_PREFIX: &str = "bootstrap:";

const GOOGLE_ICON: &str = r##"<svg viewBox="0 0 18 18" role="img" aria-label="Google"><path fill="#4285f4" d="M17.64 9.2c0-.64-.06-1.25-.16-1.84H9v3.48h4.84a4.14 4.14 0 0 1-1.8 2.72v2.26h2.91c1.7-1.57 2.69-3.88 2.69-6.62z"/><path fill="#34a853" d="M9 18c2.43 0 4.47-.8 5.96-2.18l-2.91-2.26c-.81.54-1.84.86-3.05.86a5.38 5.38 0 0 1-5.06-3.72H.93v2.33A9 9 0 0 0 9 18z"/><path fill="#fbbc05" d="M3.94 10.7A5.4 5.4 0 0 1 3.66 9c0-.59.1-1.16.28-1.7V4.97H.93A9 9 0 0 0 0 9c0 1.45.34 2.82.93 4.03l3.01-2.33z"/><path fill="#ea4335" d="M9 3.58c1.32 0 2.51.45 3.44 1.35l2.58-2.58A8.65 8.65 0 0 0 9 0 9 9 0 0 0 .93 4.97L3.94 7.3A5.38 5.38 0 0 1 9 3.58z"/></svg>"##;
const MICROSOFT_ICON: &str = r##"<svg viewBox="0 0 23 23" role="img" aria-label="Microsoft"><path fill="#f25022" d="M1 1h10v10H1z"/><path fill="#7fba00" d="M12 1h10v10H12z"/><path fill="#00a4ef" d="M1 12h10v10H1z"/><path fill="#ffb900" d="M12 12h10v10H12z"/></svg>"##;
const APPLE_ICON: &str = r##"<svg viewBox="0 0 24 24" role="img" aria-label="Apple"><path fill="currentColor" d="M16.37 1.43c0 1.08-.4 2.03-1.18 2.86-.86.91-1.9 1.44-3.02 1.36-.13-1.04.43-2.13 1.18-2.94.82-.89 2.23-1.56 3.02-1.28zM20.3 17.48c-.5 1.17-.74 1.69-1.38 2.72-.9 1.46-2.17 3.28-3.75 3.3-1.4.01-1.76-.95-3.67-.94-1.9 0-2.3.96-3.69.95-1.58-.02-2.78-1.66-3.69-3.12-2.52-4.08-2.79-8.87-1.23-11.42 1.11-1.82 2.86-2.88 4.5-2.88 1.67 0 2.73.97 4.11.97 1.34 0 2.15-.97 4.08-.97 1.45 0 3 .82 4.1 2.24-3.6 2.08-3.02 7.44.62 9.15z"/></svg>"##;
const GENERIC_ICON: &str = r##"<svg viewBox="0 0 24 24" role="img" aria-label="Identity provider"><path fill="currentColor" d="M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zm0 2c-4.42 0-8 2.24-8 5v1h16v-1c0-2.76-3.58-5-8-5z"/></svg>"##;

#[derive(Debug, Deserialize)]
pub struct BootstrapTokenForm {
    label: Option<String>,
}

pub async fn page(
    State(store): State<Store>,
    State(bootstrap): State<Option<BootstrapConfig>>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    Path(code): Path<String>,
) -> Result<Html<String>, ApiError> {
    let bootstrap = ensure_bootstrap_armed(bootstrap)?;
    ensure_access_code(&bootstrap, &code)?;
    if !store.first_credential_bootstrap_available().await? {
        return Ok(Html(completed_page()));
    }
    Ok(Html(setup_page(&bootstrap, runtime.as_deref())))
}

pub async fn issue_bearer(
    State(store): State<Store>,
    State(bootstrap): State<Option<BootstrapConfig>>,
    Path(code): Path<String>,
    Form(form): Form<BootstrapTokenForm>,
) -> Result<Html<String>, ApiError> {
    let bootstrap = ensure_bootstrap_armed(bootstrap)?;
    ensure_access_code(&bootstrap, &code)?;

    let label = form
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| truncate_chars(label, BOOTSTRAP_TOKEN_LABEL_MAX_LEN))
        .unwrap_or(BOOTSTRAP_TOKEN_LABEL);
    let issued = store
        .issue_first_bootstrap_token(label)
        .await?
        .ok_or_else(|| ApiError::conflict("first credential bootstrap is no longer available"))?;
    tracing::info!("first bootstrap bearer token issued");
    Ok(Html(bearer_token_page(&issued.token, &bootstrap)))
}

pub async fn start_oauth(
    State(store): State<Store>,
    State(bootstrap): State<Option<BootstrapConfig>>,
    State(runtime): State<Option<Arc<OAuthRuntime>>>,
    Path((code, provider_name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let bootstrap = ensure_bootstrap_armed(bootstrap)?;
    ensure_access_code(&bootstrap, &code)?;
    if !store.first_credential_bootstrap_available().await? {
        return Err(ApiError::conflict(
            "first credential bootstrap is no longer available",
        ));
    }
    let Some(runtime) = runtime else {
        return Err(ApiError::not_found("oauth is disabled"));
    };
    let Some(provider) = runtime.provider(&provider_name) else {
        return Err(ApiError::bad_request("unknown or disabled provider"));
    };

    let upstream_state = format!(
        "{BOOTSTRAP_STATE_PREFIX}{}",
        tokens::generate_opaque_value()
    );
    let upstream_nonce = tokens::generate_opaque_value();
    let expires_at = Utc::now() + AUTHORIZATION_REQUEST_TTL;
    let callback_uri = oauth_callback_url(&runtime, &provider_name);
    store
        .create_authorization_request(&NewAuthorizationRequest {
            client_id: BOOTSTRAP_CLIENT_ID,
            redirect_uri: BOOTSTRAP_REDIRECT_URI,
            code_challenge: BOOTSTRAP_CODE_CHALLENGE,
            code_challenge_method: "S256",
            client_state: None,
            provider: &provider_name,
            upstream_state: &upstream_state,
            upstream_nonce: &upstream_nonce,
            expires_at,
        })
        .await?;

    let redirect_url = provider.authorize_url(&upstream_state, &upstream_nonce, &callback_uri);
    Ok(Redirect::to(&redirect_url).into_response())
}

pub struct BootstrapOauthCallback<'a> {
    pub store: &'a Store,
    pub bootstrap: Option<BootstrapConfig>,
    pub runtime: &'a OAuthRuntime,
    pub provider_name: &'a str,
    pub provider: &'a Arc<dyn OidcProvider>,
    pub code: &'a str,
    pub state: &'a str,
    pub peer: SocketAddr,
}

pub async fn complete_oauth_callback(
    ctx: BootstrapOauthCallback<'_>,
) -> Result<Response, ApiError> {
    let bootstrap = ensure_bootstrap_allowed_from(ctx.bootstrap, ctx.peer)?;
    ensure_bootstrap_unlocked(&bootstrap)?;

    let request = ctx
        .store
        .find_authorization_request_by_upstream_state(ctx.provider_name, ctx.state)
        .await?
        .ok_or_else(|| ApiError::bad_request("unknown or expired bootstrap flow"))?;
    if request.client_id != BOOTSTRAP_CLIENT_ID || request.redirect_uri != BOOTSTRAP_REDIRECT_URI {
        return Err(ApiError::bad_request("unknown or expired bootstrap flow"));
    }

    let callback_uri = oauth_callback_url(ctx.runtime, ctx.provider_name);
    let upstream = ctx
        .provider
        .exchange_code(ctx.code, &callback_uri)
        .await
        .map_err(oidc_error_to_api_error)?;
    let verified = ctx
        .provider
        .verify_identity_token(&upstream.id_token, Some(&request.upstream_nonce))
        .await
        .map_err(oidc_error_to_api_error)?;

    let now = Utc::now();
    let access_expires_at = now + chrono_duration(ctx.runtime.access_token_ttl)?;
    let refresh_expires_at = now + chrono_duration(ctx.runtime.refresh_token_ttl)?;
    let issued = ctx
        .store
        .issue_first_oauth_token_pair(
            ctx.provider_name,
            &verified.subject,
            verified.email.as_deref(),
            BOOTSTRAP_CLIENT_ID,
            access_expires_at,
            refresh_expires_at,
        )
        .await?
        .ok_or_else(|| ApiError::conflict("first credential bootstrap is no longer available"))?;
    tracing::info!(
        provider = ctx.provider_name,
        "first bootstrap SSO credential issued"
    );

    Ok(Html(oauth_token_page(
        TokenPair {
            access_token: issued.access_token,
            refresh_token: issued.refresh_token,
            expires_in: ctx.runtime.access_token_ttl.as_secs(),
        },
        &bootstrap,
    ))
    .into_response())
}

pub async fn require_allowed_peer(
    State(bootstrap): State<Option<BootstrapConfig>>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, ApiError> {
    if let Some(bootstrap) = bootstrap {
        if !bootstrap.allow_remote
            && req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .is_some_and(|ConnectInfo(addr)| !addr.ip().is_loopback())
        {
            return Err(ApiError::forbidden(
                "web bootstrap is only available from loopback",
            ));
        }
    }
    Ok(next.run(req).await)
}

pub async fn wrong_url(
    State(bootstrap): State<Option<BootstrapConfig>>,
) -> Result<Response, ApiError> {
    let bootstrap = ensure_bootstrap_armed(bootstrap)?;
    match bootstrap.validate_access_code("") {
        Ok(()) => unreachable!("the empty string is never a generated bootstrap access code"),
        Err(crate::state::BootstrapAccessCodeError::Invalid) => {
            tracing::warn!(
                failed_attempts = bootstrap.failed_url_attempts(),
                max_attempts = crate::state::BOOTSTRAP_MAX_WRONG_URLS,
                "wrong bootstrap URL requested"
            );
            Err(ApiError::not_found("web bootstrap URL not found"))
        }
        Err(crate::state::BootstrapAccessCodeError::Locked) => {
            tracing::warn!(
                failed_attempts = bootstrap.failed_url_attempts(),
                max_attempts = crate::state::BOOTSTRAP_MAX_WRONG_URLS,
                "web bootstrap locked after wrong URL attempts"
            );
            Err(ApiError::too_many_requests(
                "web bootstrap locked after too many wrong URLs",
            ))
        }
    }
}

fn ensure_bootstrap_armed(bootstrap: Option<BootstrapConfig>) -> Result<BootstrapConfig, ApiError> {
    bootstrap.ok_or_else(|| ApiError::not_found("web bootstrap is not armed"))
}

fn ensure_bootstrap_allowed_from(
    bootstrap: Option<BootstrapConfig>,
    peer: SocketAddr,
) -> Result<BootstrapConfig, ApiError> {
    let bootstrap = ensure_bootstrap_armed(bootstrap)?;
    if !bootstrap.allow_remote && !peer.ip().is_loopback() {
        return Err(ApiError::forbidden(
            "web bootstrap is only available from loopback",
        ));
    }
    Ok(bootstrap)
}

fn ensure_bootstrap_unlocked(bootstrap: &BootstrapConfig) -> Result<(), ApiError> {
    match bootstrap.ensure_unlocked() {
        Ok(()) => Ok(()),
        Err(crate::state::BootstrapLocked) => Err(ApiError::too_many_requests(
            "web bootstrap locked after too many wrong URLs",
        )),
    }
}

fn ensure_access_code(bootstrap: &BootstrapConfig, submitted: &str) -> Result<(), ApiError> {
    match bootstrap.validate_access_code(submitted) {
        Ok(()) => Ok(()),
        Err(crate::state::BootstrapAccessCodeError::Invalid) => {
            tracing::warn!(
                failed_attempts = bootstrap.failed_url_attempts(),
                max_attempts = crate::state::BOOTSTRAP_MAX_WRONG_URLS,
                "wrong bootstrap URL requested"
            );
            Err(ApiError::not_found("web bootstrap URL not found"))
        }
        Err(crate::state::BootstrapAccessCodeError::Locked) => {
            tracing::warn!(
                failed_attempts = bootstrap.failed_url_attempts(),
                max_attempts = crate::state::BOOTSTRAP_MAX_WRONG_URLS,
                "web bootstrap locked after wrong URL attempts"
            );
            Err(ApiError::too_many_requests(
                "web bootstrap locked after too many wrong URLs",
            ))
        }
    }
}

fn oauth_callback_url(runtime: &OAuthRuntime, provider_name: &str) -> String {
    format!(
        "{}/v1/oauth/{provider_name}/callback",
        runtime.external_base_url
    )
}

fn chrono_duration(duration: std::time::Duration) -> Result<ChronoDuration, ApiError> {
    ChronoDuration::from_std(duration).map_err(|_| ApiError::internal())
}

fn oidc_error_to_api_error(err: OidcError) -> ApiError {
    tracing::warn!(error = %err, "oidc verification failed during bootstrap");
    ApiError::bad_gateway("upstream identity verification failed")
}

fn setup_page(bootstrap: &BootstrapConfig, runtime: Option<&OAuthRuntime>) -> String {
    let mut provider_links = String::new();
    if let Some(runtime) = runtime {
        let mut providers = runtime.providers.keys().copied().collect::<Vec<_>>();
        providers.sort_unstable();
        for provider in providers {
            provider_links.push_str(&provider_button(provider, bootstrap.access_code()));
        }
    }
    if provider_links.is_empty() {
        provider_links
            .push_str(r#"<p class="muted">SSO is not configured for this Axon instance.</p>"#);
    }

    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Set up Axon</title>
    <style>{CSS}</style>
  </head>
  <body>
    <main>
      <h1>Set up Axon</h1>
      <p>Axon is a self-hosted personal agent for Matrix. It sits between your homeserver(s) and your clients, providing the persistent state, full-text search index, and per-device coherence that Matrix clients otherwise have to reinvent on every install.</p>
      <p>This instance has no accounts or client credentials yet. Before you can add any Matrix accounts to Axon, you need to create an Axon credential. This credential allows you to authenticate to the Axon server from any Axon client. You can then add any number of Matrix accounts to your Axon instance.</p>
      <p>There are currently two methods to create credentials from this bootstrap interface. You can mint a bearer token, which you will then enter into your Axon web or terminal client. You can alternatively use Google single-sign-on, which you can then use from any Axon client to access your Axon instance.</p>
      <p>If you use the single-sign-on option, you do need to record your access token, since that will be handled transparently by the single-sign-on process.</p>
      <section>
        <h2>Single Sign On</h2>
        <div class="actions">{provider_links}</div>
      </section>
      <section>
        <h2>Bearer token</h2>
        <form method="post" action="/bootstrap/{code}/token">
          <label>Label <input name="label" maxlength="80" value="{default_label}"></label>
          <button type="submit">Create bearer token</button>
        </form>
      </section>
    </main>
  </body>
</html>"#,
        code = html_escape(bootstrap.access_code()),
        default_label = BOOTSTRAP_TOKEN_LABEL,
    )
}

fn provider_button(provider: &str, code: &str) -> String {
    let href = format!(
        "/bootstrap/{code}/oauth/{provider}",
        code = html_escape(code),
        provider = html_escape(provider),
    );
    let (class, label, icon) = match provider {
        "google" => ("sso-button sso-google", "Continue with Google", GOOGLE_ICON),
        "microsoft" => (
            "sso-button sso-microsoft",
            "Sign in with Microsoft",
            MICROSOFT_ICON,
        ),
        "apple" => ("sso-button sso-apple", "Sign in with Apple", APPLE_ICON),
        other => {
            return format!(
                r#"<a class="sso-button sso-generic" href="{href}"><span class="sso-icon" aria-hidden="true">{GENERIC_ICON}</span><span>Continue with {}</span></a>"#,
                html_escape(other)
            );
        }
    };
    format!(
        r#"<a class="{class}" href="{href}"><span class="sso-icon" aria-hidden="true">{icon}</span><span>{label}</span></a>"#
    )
}

fn web_client_link(bootstrap: &BootstrapConfig) -> String {
    let Some(url) = bootstrap.web_client_url.as_deref() else {
        return String::new();
    };
    format!(
        r#"<p><a class="button" href="{url}">Open web client</a></p>"#,
        url = html_escape(url)
    )
}

/// The web client's `localStorage` slot for a bearer token — the exact key the
/// token-paste auth provider reads on load (`clients/web/src/auth/token-paste.tsx`,
/// `STORAGE_KEY`). The same-origin handoff below writes here so the SPA comes up
/// already signed in; keep it in sync with the client.
const WEB_CLIENT_TOKEN_STORAGE_KEY: &str = "axon.token";

/// Same-origin handoff for the web client (ADR 0052). When `web_client_url`
/// resolves to *this* origin — the case behind the deployment front door, which
/// serves the SPA and proxies `/bootstrap` on one origin — write the freshly
/// minted token into the client's `localStorage` slot and redirect there, so the
/// operator lands signed in with nothing to copy. The check runs in the browser
/// (`new URL(...).origin`), so a cross-origin or JS-disabled client simply falls
/// back to the copyable token and the "Open web client" link above.
fn web_client_handoff_script(bootstrap: &BootstrapConfig) -> String {
    let Some(url) = bootstrap.web_client_url.as_deref() else {
        return String::new();
    };
    format!(
        r#"<div id="axon-bootstrap" data-web-client-url="{url}" hidden></div>
      <script>(function () {{
  var host = document.getElementById('axon-bootstrap');
  var tokenEl = document.getElementById('axon-bootstrap-token');
  if (!host || !tokenEl) return;
  var url = host.dataset.webClientUrl;
  if (!url) return;
  try {{
    if (new URL(url, location.href).origin !== location.origin) return;
    localStorage.setItem('{key}', tokenEl.textContent.trim());
    location.replace(url);
  }} catch (e) {{}}
}})();</script>"#,
        url = html_escape(url),
        key = WEB_CLIENT_TOKEN_STORAGE_KEY,
    )
}

fn bearer_token_page(token: &str, bootstrap: &BootstrapConfig) -> String {
    let client_link = web_client_link(bootstrap);
    let handoff = web_client_handoff_script(bootstrap);
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Axon token created</title>
    <style>{CSS}</style>
  </head>
  <body>
    <main>
      <h1>Bearer token created</h1>
      <p>Copy this token now. Axon stores only its hash and cannot show it again.</p>
      <pre id="axon-bootstrap-token">{token}</pre>
      {client_link}
      {handoff}
    </main>
  </body>
</html>"#,
        token = html_escape(token),
    )
}

fn oauth_token_page(pair: TokenPair, bootstrap: &BootstrapConfig) -> String {
    let client_link = web_client_link(bootstrap);
    // No same-origin localStorage hand-off here (unlike `bearer_token_page`): the
    // OAuth access token is short-lived (`expires_in`) and the web client's
    // token-paste slot stores a single bearer with no refresh handling, so
    // silently landing it there would sign the operator out on expiry. SSO users
    // copy the tokens (or use the web client's own OAuth/PKCE flow, which manages
    // refresh); only the long-lived bearer path gets the zero-paste hand-off.
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Axon SSO configured</title>
    <style>{CSS}</style>
  </head>
  <body>
    <main>
      <h1>SSO configured</h1>
      <p>Copy these tokens now. Axon stores only their hashes and cannot show them again.</p>
      <label>Access token</label>
      <pre>{}</pre>
      <label>Refresh token</label>
      <pre>{}</pre>
      <p class="muted">Access token expires in {} seconds.</p>
      {client_link}
    </main>
  </body>
</html>"#,
        html_escape(&pair.access_token),
        html_escape(&pair.refresh_token),
        pair.expires_in
    )
}

/// Truncate to at most `max_chars` `char`s, cutting on a char boundary
/// (unlike a raw byte-length slice, which would panic on a multi-byte char
/// straddling the cut point).
fn truncate_chars(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &value[..byte_idx],
        None => value,
    }
}

pub(crate) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn completed_page() -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Axon bootstrap complete</title>
    <style>{CSS}</style>
  </head>
  <body>
    <main>
      <h1>Bootstrap complete</h1>
      <p>This Axon instance already has an account or login credential. The web bootstrap is closed.</p>
    </main>
  </body>
</html>"#
    )
}

const CSS: &str = r#"
:root { color-scheme: light dark; font-family: system-ui, sans-serif; }
body { margin: 0; background: Canvas; color: CanvasText; }
main { max-width: 720px; margin: 8vh auto; padding: 0 24px; }
h1 { font-size: 2rem; margin-bottom: 8px; }
h2 { font-size: 1.1rem; margin-top: 28px; }
section { border-top: 1px solid color-mix(in srgb, CanvasText 18%, transparent); padding-top: 18px; }
label { display: block; font-weight: 600; margin-bottom: 10px; }
input { display: block; width: min(420px, 100%); margin-top: 6px; padding: 8px; font: inherit; }
button, .button { display: inline-block; border: 0; border-radius: 6px; padding: 10px 14px; background: #116a64; color: white; font: inherit; text-decoration: none; cursor: pointer; }
.secondary { background: #334155; }
.actions { display: flex; flex-direction: column; align-items: flex-start; gap: 10px; }
.sso-button {
  box-sizing: border-box;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: min(320px, 100%);
  min-height: 44px;
  border: 1px solid transparent;
  border-radius: 4px;
  padding: 0 16px;
  gap: 12px;
  font: 500 14px/20px "Google Sans", Roboto, Arial, sans-serif;
  text-decoration: none;
}
.sso-button:hover { filter: brightness(0.97); }
.sso-button:focus-visible { outline: 3px solid #7aa2ff; outline-offset: 2px; }
.sso-icon { display: inline-flex; flex: 0 0 20px; width: 20px; height: 20px; }
.sso-icon svg { display: block; width: 20px; height: 20px; }
.sso-google { background: #fff; border-color: #747775; color: #1f1f1f; }
.sso-microsoft { background: #fff; border-color: #8c8c8c; color: #5e5e5e; }
.sso-apple { background: #000; border-color: #000; color: #fff; }
.sso-generic { background: #334155; border-color: #334155; color: #fff; }
pre { white-space: pre-wrap; overflow-wrap: anywhere; padding: 16px; border-radius: 6px; background: color-mix(in srgb, CanvasText 8%, transparent); }
.muted { color: color-mix(in srgb, CanvasText 72%, transparent); }
"#;

#[cfg(test)]
mod tests {
    use super::{
        bearer_token_page, oauth_token_page, provider_button, truncate_chars, BootstrapConfig,
    };
    use crate::oauth::tokens::TokenPair;

    #[test]
    fn truncate_chars_cuts_at_char_boundary() {
        assert_eq!(truncate_chars("hello", 80), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello", 3), "hel");
        assert_eq!(truncate_chars("hello", 0), "");
        // Multi-byte chars: cutting mid-string must not panic or split a char.
        let multibyte = "a".repeat(79) + "\u{1F600}\u{1F600}";
        let truncated = truncate_chars(&multibyte, 80);
        assert_eq!(truncated.chars().count(), 80);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn bearer_token_page_escapes_the_token() {
        let bootstrap = BootstrapConfig::new(false, "ABC234".to_owned(), None);
        let page = bearer_token_page("<script>evil</script>", &bootstrap);
        assert!(!page.contains("<script>evil</script>"));
        assert!(page.contains("&lt;script&gt;evil&lt;/script&gt;"));
    }

    #[test]
    fn bearer_token_page_omits_handoff_without_web_client_url() {
        let bootstrap = BootstrapConfig::new(false, "ABC234".to_owned(), None);
        let page = bearer_token_page("tok", &bootstrap);
        assert!(!page.contains("localStorage"));
        assert!(!page.contains("data-web-client-url"));
    }

    #[test]
    fn bearer_token_page_adds_same_origin_handoff_with_web_client_url() {
        let bootstrap = BootstrapConfig::new(false, "ABC234".to_owned(), Some("/".to_owned()));
        let page = bearer_token_page("tok", &bootstrap);
        // The token holder the handoff script reads, the storage key it writes,
        // and the origin guard are all present.
        assert!(page.contains(r#"id="axon-bootstrap-token""#));
        assert!(page.contains("localStorage.setItem('axon.token'"));
        assert!(page.contains("location.origin"));
    }

    #[test]
    fn bearer_token_page_escapes_the_web_client_url() {
        let bootstrap = BootstrapConfig::new(
            false,
            "ABC234".to_owned(),
            Some(r#"https://x/"><script>evil</script>"#.to_owned()),
        );
        let page = bearer_token_page("tok", &bootstrap);
        assert!(!page.contains(r#""><script>evil</script>"#));
        assert!(page.contains("&lt;script&gt;evil&lt;/script&gt;"));
    }

    #[test]
    fn oauth_token_page_escapes_both_tokens() {
        let bootstrap = BootstrapConfig::new(false, "ABC234".to_owned(), None);
        let page = oauth_token_page(
            TokenPair {
                access_token: "<b>access</b>".to_owned(),
                refresh_token: "<b>refresh</b>".to_owned(),
                expires_in: 3600,
            },
            &bootstrap,
        );
        assert!(!page.contains("<b>access</b>"));
        assert!(!page.contains("<b>refresh</b>"));
        assert!(page.contains("&lt;b&gt;access&lt;/b&gt;"));
        assert!(page.contains("&lt;b&gt;refresh&lt;/b&gt;"));
    }

    #[test]
    fn provider_buttons_use_expected_branding() {
        let google = provider_button("google", "secret");
        assert!(google.contains("sso-google"));
        assert!(google.contains("Continue with Google"));
        assert!(google.contains("aria-label=\"Google\""));

        let microsoft = provider_button("microsoft", "secret");
        assert!(microsoft.contains("sso-microsoft"));
        assert!(microsoft.contains("Sign in with Microsoft"));
        assert!(microsoft.contains("aria-label=\"Microsoft\""));

        let apple = provider_button("apple", "secret");
        assert!(apple.contains("sso-apple"));
        assert!(apple.contains("Sign in with Apple"));
        assert!(apple.contains("aria-label=\"Apple\""));
    }

    #[test]
    fn unknown_provider_button_is_escaped() {
        let button = provider_button("<test>", "A2B3C4");
        assert!(button.contains("Continue with &lt;test&gt;"));
        assert!(button.contains("/bootstrap/A2B3C4/oauth/&lt;test&gt;"));
    }
}
