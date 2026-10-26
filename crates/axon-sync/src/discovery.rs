//! Server-side homeserver discovery (client-server spec, "Server Discovery").
//!
//! Given a Matrix user ID's server name, resolve the canonical homeserver base
//! URL: fetch `https://<server name>/.well-known/matrix/client` and use its
//! `m.homeserver.base_url`, falling back to `https://<server name>` when no
//! document is published. Either candidate must pass validation — HTTPS
//! (plain HTTP is allowed only when the MXID server name itself is loopback)
//! and a non-empty `GET /_matrix/client/versions` — before it is accepted.
//!
//! Discovery lives here, not in clients, so egress to user-named hosts stays
//! out of every client and exactly one canonical `homeserver_url` keys each
//! identity — two clients resolving the same user differently would otherwise
//! mint duplicate accounts (the natural key is `(user_id, homeserver_url)`).
//! [`check_user_id_domain`] guards the *user-ID half* of that key: an MXID
//! whose domain is actually the homeserver's hostname is rejected with a
//! did-you-mean error naming the canonical spelling, so the wrong spelling can
//! never key (or fail to converge with) an account row.
//!
//! Spec mapping (well-known fetch): an unreachable host, a non-2xx, or a 2xx
//! whose body isn't JSON is treated like the spec's 404 `IGNORE` — fall back
//! to the server name itself (a non-JSON 200 is a server that 200s every path,
//! e.g. a Caddy/SPA/default-vhost catch-all, not a published matrix document;
//! and the fallback probes the same origin, so a truly dead host still fails,
//! just at the versions check). A 2xx *JSON* document that lacks a usable
//! `m.homeserver.base_url`, or names an invalid homeserver, is an error
//! (`FAIL_PROMPT`/`FAIL_ERROR`): someone published a matrix well-known and got
//! it wrong, so silently guessing would log the user into a homeserver their
//! domain didn't name.

use std::time::Duration;

use matrix_sdk::reqwest;
use matrix_sdk::ruma::{OwnedUserId, ServerName, UserId};

/// Cap on each discovery HTTP request (well-known fetch, versions probe), so a
/// stalled user-named host can't hold a login request open indefinitely.
const DISCOVERY_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// What can go wrong resolving a homeserver or checking a user ID against it.
/// The resolution variants surface to the login caller as an upstream (502)
/// failure; [`WrongUserIdDomain`](Self::WrongUserIdDomain) is the caller's
/// mistake and surfaces as a 400. The variants are distinct so the message says
/// *which* step refused.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DiscoveryError {
    /// The well-known document came back as 2xx JSON but is missing a usable
    /// `m.homeserver.base_url`.
    #[error("{server_name} publishes an invalid .well-known/matrix/client document: {detail}")]
    InvalidWellKnown { server_name: String, detail: String },
    /// A candidate base URL failed validation (bad scheme, unreachable, or not
    /// a Matrix homeserver).
    #[error("homeserver discovery for {server_name} failed: {detail}")]
    InvalidHomeserver { server_name: String, detail: String },
    /// The user ID's domain is the homeserver's hostname, not the server name
    /// its user IDs actually use — no such user can exist there, so reject with
    /// the spelling they almost certainly meant.
    #[error("{typed} uses the homeserver's hostname as its domain; user IDs on this homeserver end in :{declared} — did you mean {suggestion}?")]
    WrongUserIdDomain {
        typed: OwnedUserId,
        declared: matrix_sdk::OwnedServerName,
        suggestion: OwnedUserId,
    },
}

/// The HTTP client discovery uses: short per-request timeout, otherwise stock.
/// Built once by the lifecycle and reused across logins.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::client::user_agent())
        .timeout(DISCOVERY_HTTP_TIMEOUT)
        .build()
        .expect("discovery HTTP client must build")
}

/// Resolve the canonical homeserver base URL for `server_name` (no trailing
/// slash). Well-known first, then the server name itself; the winning candidate
/// has answered `GET /_matrix/client/versions`.
pub(crate) async fn resolve_homeserver(
    http: &reqwest::Client,
    server_name: &ServerName,
) -> Result<String, DiscoveryError> {
    let origin = origin_for(server_name);

    if let Some(base_url) = fetch_well_known(http, server_name, &origin).await? {
        validate_homeserver(http, server_name, &base_url).await?;
        return Ok(base_url);
    }

    validate_homeserver(http, server_name, &origin).await?;
    Ok(origin)
}

/// Normalize and accept an explicitly-supplied `homeserver_url` (login's escape
/// hatch for homeservers without — or with broken — well-known, and the only
/// way to reach a plain-HTTP loopback dev homeserver). Trims a trailing slash so
/// it keys identically to a discovered URL — the natural key is
/// `(user_id, homeserver_url)`, and a stray slash would mint a *second* row for
/// an identity discovery already keys without one — and enforces the same
/// HTTPS-unless-loopback scheme rule discovery applies to its own candidates,
/// without which the escape hatch could ship a password in cleartext to a
/// plain-HTTP public host. Unlike a discovered candidate it is **not** probed
/// (`GET /_matrix/client/versions`): the URL is caller-asserted — there is no
/// guess to confirm — and a genuinely-bad one surfaces at the SDK login call.
/// `server_name` is only the typed MXID's domain, used for error context.
pub(crate) fn accept_explicit_homeserver(
    server_name: &ServerName,
    url: &str,
) -> Result<String, DiscoveryError> {
    let base_url = url.trim_end_matches('/').to_owned();
    check_explicit_scheme(server_name, &base_url)?;
    Ok(base_url)
}

/// Check a user ID's domain against the homeserver's own declared server name.
/// Users routinely type the homeserver host where their MXID's domain is the
/// server name (`@adam:matrix.example.org` for `@adam:example.org`), which
/// would otherwise fail with a misleading "authentication failed" — the
/// password is right, the identity doesn't exist. Rejecting with the spelling
/// they meant ([`DiscoveryError::WrongUserIdDomain`]) turns that into a
/// self-explanatory 400. We deliberately do **not** log them in under the
/// corrected spelling: a login should never succeed as an identity other than
/// the one the caller typed.
///
/// The homeserver declares its server name in `GET /_matrix/key/v2/server`
/// (unauthenticated; technically federation surface, but Synapse and Dendrite
/// serve it on the client listener too — hence **best-effort**: an absent or
/// unusable key endpoint passes the user ID as typed). The rejection is safe by
/// construction: it fires only when `base_url`'s server says its users live
/// under a *different* domain — in which case the typed MXID cannot exist on it,
/// so nothing legitimate is refused.
pub(crate) async fn check_user_id_domain(
    http: &reqwest::Client,
    base_url: &str,
    user_id: &UserId,
) -> Result<(), DiscoveryError> {
    let Some(declared) = declared_server_name(http, base_url).await else {
        return Ok(());
    };
    if declared == *user_id.server_name() {
        return Ok(());
    }
    match OwnedUserId::try_from(format!("@{}:{declared}", user_id.localpart())) {
        Ok(suggestion) => Err(DiscoveryError::WrongUserIdDomain {
            typed: user_id.to_owned(),
            declared,
            suggestion,
        }),
        // A declared server name that can't form a user ID — we can't suggest
        // anything sensible, so let the homeserver's own rejection speak.
        Err(err) => {
            tracing::warn!(%declared, error = %err, "homeserver declared an unusable server name; passing the user id through as typed");
            Ok(())
        }
    }
}

/// The server name `base_url`'s homeserver declares for itself, or `None` if it
/// doesn't expose `/_matrix/key/v2/server` (or answers garbage).
async fn declared_server_name(
    http: &reqwest::Client,
    base_url: &str,
) -> Option<matrix_sdk::OwnedServerName> {
    let response = http
        .get(format!("{base_url}/_matrix/key/v2/server"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let document = read_json(response).await.ok()?;
    document["server_name"]
        .as_str()
        .and_then(|name| matrix_sdk::OwnedServerName::try_from(name).ok())
}

/// The URL origin for `server_name`: HTTPS, except loopback names (which serve
/// plain HTTP in local dev and tests).
fn origin_for(server_name: &ServerName) -> String {
    let scheme = if is_loopback_host(server_name.host()) {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{server_name}")
}

fn is_loopback_host(host: &str) -> bool {
    // ruma keeps IPv6 hosts bracketed (`[::1]`); strip for IpAddr parsing.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Fetch `<origin>/.well-known/matrix/client`. `Ok(None)` means "no well-known
/// published, fall back": unreachable, non-2xx, or a 2xx whose body isn't JSON
/// — servers that 200 every path (Caddy/SPA/default-vhost catch-alls) answer
/// 200 with an empty or HTML body, which is not a published document. A 2xx
/// *JSON* document that doesn't yield a base URL is an error, not a fallback.
async fn fetch_well_known(
    http: &reqwest::Client,
    server_name: &ServerName,
    origin: &str,
) -> Result<Option<String>, DiscoveryError> {
    let url = format!("{origin}/.well-known/matrix/client");
    let response = match http.get(url).send().await {
        Ok(response) if response.status().is_success() => response,
        _ => return Ok(None),
    };
    let Ok(document) = read_json(response).await else {
        return Ok(None);
    };

    let base_url = document["m.homeserver"]["base_url"]
        .as_str()
        .ok_or_else(|| DiscoveryError::InvalidWellKnown {
            server_name: server_name.to_string(),
            detail: "missing m.homeserver.base_url".to_owned(),
        })?;
    Ok(Some(base_url.trim_end_matches('/').to_owned()))
}

/// Read a response body as JSON. (Not `Response::json` — that needs reqwest's
/// `json` feature, which this crate would otherwise only inherit by accident of
/// workspace feature unification.)
async fn read_json(response: reqwest::Response) -> Result<serde_json::Value, String> {
    let body = response.text().await.map_err(|err| err.to_string())?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

/// Enforce a sane scheme on `base_url`: HTTPS, except plain HTTP to a loopback
/// host when `allow_http_loopback` is true. Explicit overrides allow that local
/// dev escape hatch regardless of the MXID domain; discovery enables it only
/// when the MXID server name itself is loopback, so a public well-known cannot
/// redirect credentials into axon's own loopback. Pure parse work, no network.
fn check_scheme(
    server_name: &ServerName,
    base_url: &str,
    allow_http_loopback: bool,
) -> Result<(), DiscoveryError> {
    let invalid = |detail: String| DiscoveryError::InvalidHomeserver {
        server_name: server_name.to_string(),
        detail,
    };
    let parsed = reqwest::Url::parse(base_url)
        .map_err(|err| invalid(format!("invalid base URL {base_url}: {err}")))?;
    let loopback = parsed.host_str().is_some_and(is_loopback_host);
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback && allow_http_loopback)
    {
        return Err(invalid(format!(
            "{base_url} must use HTTPS unless plain-HTTP loopback is explicitly allowed"
        )));
    }
    Ok(())
}

fn check_explicit_scheme(server_name: &ServerName, base_url: &str) -> Result<(), DiscoveryError> {
    check_scheme(server_name, base_url, true)
}

fn check_discovered_scheme(server_name: &ServerName, base_url: &str) -> Result<(), DiscoveryError> {
    check_scheme(server_name, base_url, is_loopback_host(server_name.host()))
}

/// Accept `base_url` only if it has a sane scheme (HTTPS, or HTTP to loopback
/// when the MXID server name is itself loopback) and answers
/// `GET /_matrix/client/versions` with a non-empty version list.
async fn validate_homeserver(
    http: &reqwest::Client,
    server_name: &ServerName,
    base_url: &str,
) -> Result<(), DiscoveryError> {
    check_discovered_scheme(server_name, base_url)?;
    let invalid = |detail: String| DiscoveryError::InvalidHomeserver {
        server_name: server_name.to_string(),
        detail,
    };

    let response = http
        .get(format!("{base_url}/_matrix/client/versions"))
        .send()
        .await
        .map_err(|err| invalid(format!("{base_url} is unreachable: {err}")))?;
    if !response.status().is_success() {
        return Err(invalid(format!(
            "{base_url} versions check returned HTTP {}",
            response.status()
        )));
    }
    let versions = read_json(response)
        .await
        .map_err(|err| invalid(format!("{base_url} versions response invalid: {err}")))?;
    if versions["versions"]
        .as_array()
        .is_none_or(|list| list.is_empty())
    {
        return Err(invalid(format!(
            "{base_url} reports no supported client versions"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::routing::get;
    use axum::{Json, Router};
    use matrix_sdk::ruma::OwnedServerName;
    use serde_json::json;

    use super::*;

    /// Serve `router` on an ephemeral loopback port; the task lives for the
    /// duration of the test process.
    async fn serve(router: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    /// `host:port` of a test server as a Matrix server name (loopback, so
    /// discovery probes it over plain HTTP).
    fn server_name(addr: SocketAddr) -> OwnedServerName {
        OwnedServerName::try_from(addr.to_string().as_str()).unwrap()
    }

    /// A homeserver that answers the versions probe.
    fn versions_router() -> Router {
        Router::new().route(
            "/_matrix/client/versions",
            get(|| async { Json(json!({ "versions": ["v1.18"] })) }),
        )
    }

    fn well_known_router(base_url: String) -> Router {
        Router::new().route(
            "/.well-known/matrix/client",
            get(move || {
                let base_url = base_url.clone();
                async move { Json(json!({ "m.homeserver": { "base_url": base_url } })) }
            }),
        )
    }

    #[tokio::test]
    async fn uses_well_known_base_url_and_normalizes_it() {
        let homeserver = serve(versions_router()).await;
        // Trailing slash: the published base_url must come out normalized,
        // since it becomes the account natural key.
        let mxdomain = serve(well_known_router(format!("http://{homeserver}/"))).await;

        let resolved = resolve_homeserver(&http_client(), &server_name(mxdomain))
            .await
            .expect("resolves via well-known");
        assert_eq!(resolved, format!("http://{homeserver}"));
    }

    #[tokio::test]
    async fn falls_back_to_server_name_when_no_well_known() {
        // The mxdomain *is* the homeserver and publishes no well-known (404).
        let addr = serve(versions_router()).await;

        let resolved = resolve_homeserver(&http_client(), &server_name(addr))
            .await
            .expect("falls back to the server name");
        assert_eq!(resolved, format!("http://{addr}"));
    }

    #[tokio::test]
    async fn unusable_well_known_document_is_an_error_not_a_fallback() {
        // The domain publishes a 2xx well-known without a base_url — and would
        // satisfy the fallback (it answers the versions probe), so a pass here
        // would mean we silently ignored what the domain explicitly published.
        let router = versions_router().route(
            "/.well-known/matrix/client",
            get(|| async { Json(json!({ "m.homeserver": {} })) }),
        );
        let addr = serve(router).await;

        let err = resolve_homeserver(&http_client(), &server_name(addr))
            .await
            .unwrap_err();
        assert!(
            matches!(err, DiscoveryError::InvalidWellKnown { .. }),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn well_known_homeserver_failing_validation_is_an_error() {
        // The well-known names a homeserver that reports no client versions.
        let homeserver = serve(Router::new().route(
            "/_matrix/client/versions",
            get(|| async { Json(json!({ "versions": [] })) }),
        ))
        .await;
        let mxdomain = serve(well_known_router(format!("http://{homeserver}"))).await;

        let err = resolve_homeserver(&http_client(), &server_name(mxdomain))
            .await
            .unwrap_err();
        assert!(
            matches!(err, DiscoveryError::InvalidHomeserver { .. }),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn non_loopback_http_base_url_is_rejected_without_probing_it() {
        // Plain HTTP is loopback-only; the scheme check runs before any network
        // probe, so this errors without egress to example.com.
        let mxdomain = serve(well_known_router("http://example.com".to_owned())).await;

        let err = resolve_homeserver(&http_client(), &server_name(mxdomain))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must use HTTPS"), "got: {err}");
    }

    #[test]
    fn public_server_name_cannot_discover_plain_http_loopback() {
        let public = OwnedServerName::try_from("example.org").unwrap();

        let err = check_discovered_scheme(&public, "http://127.0.0.1:8008").unwrap_err();
        assert!(err.to_string().contains("must use HTTPS"), "got: {err}");
    }

    /// A homeserver declaring `server_name` via `/_matrix/key/v2/server`.
    fn key_server_router(server_name: &str) -> Router {
        let body = json!({ "server_name": server_name });
        Router::new().route(
            "/_matrix/key/v2/server",
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        )
    }

    #[tokio::test]
    async fn wrong_user_id_domain_is_rejected_with_a_suggestion() {
        // The user typed the homeserver host as the MXID domain; the homeserver
        // declares its users live under example.org — reject, don't log in.
        let homeserver = serve(key_server_router("example.org")).await;
        let typed = OwnedUserId::try_from(format!("@adam:{homeserver}")).unwrap();

        let err = check_user_id_domain(&http_client(), &format!("http://{homeserver}"), &typed)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DiscoveryError::WrongUserIdDomain { .. }),
            "got: {err}"
        );
        assert!(
            err.to_string().contains("did you mean @adam:example.org?"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn matching_declared_server_name_passes() {
        let homeserver = serve(key_server_router("example.org")).await;
        let typed = OwnedUserId::try_from("@adam:example.org").unwrap();

        check_user_id_domain(&http_client(), &format!("http://{homeserver}"), &typed)
            .await
            .expect("matching domain passes");
    }

    #[tokio::test]
    async fn missing_or_garbage_key_endpoint_passes_the_user_id_as_typed() {
        // No /_matrix/key/v2/server at all (best-effort: e.g. a deployment that
        // doesn't expose federation surface on the client listener).
        let bare = serve(versions_router()).await;
        let typed = OwnedUserId::try_from(format!("@adam:{bare}")).unwrap();
        check_user_id_domain(&http_client(), &format!("http://{bare}"), &typed)
            .await
            .expect("absent key endpoint passes");

        // A 2xx document without a usable server_name.
        let garbage = serve(Router::new().route(
            "/_matrix/key/v2/server",
            get(|| async { Json(json!({ "server_name": 42 })) }),
        ))
        .await;
        let typed = OwnedUserId::try_from(format!("@adam:{garbage}")).unwrap();
        check_user_id_domain(&http_client(), &format!("http://{garbage}"), &typed)
            .await
            .expect("garbage key endpoint passes");
    }

    #[tokio::test]
    async fn non_json_200_well_known_falls_back_to_the_server_name() {
        // A server that 200s every path (Caddy/SPA/default-vhost catch-all)
        // answers the well-known probe with 200 and an empty body. That is "no
        // well-known published", not an invalid document — resolution must fall
        // back to the origin (which validates: it serves the versions probe).
        let router = versions_router().route(
            "/.well-known/matrix/client",
            get(|| async { "" }), // 200, zero-length non-JSON body
        );
        let addr = serve(router).await;

        let resolved = resolve_homeserver(&http_client(), &server_name(addr))
            .await
            .expect("empty 200 well-known is ignored, not fatal");
        assert_eq!(resolved, format!("http://{addr}"));
    }

    #[test]
    fn explicit_homeserver_is_trimmed_and_scheme_checked() {
        let sn = OwnedServerName::try_from("example.org").unwrap();

        // Trailing slash trimmed so an explicit URL keys identically to a
        // discovered one (no duplicate row for one identity).
        assert_eq!(
            accept_explicit_homeserver(&sn, "https://hs.example.org/").unwrap(),
            "https://hs.example.org"
        );
        // Plain HTTP to a loopback host: allowed (local-dev escape hatch).
        assert_eq!(
            accept_explicit_homeserver(&sn, "http://127.0.0.1:8008").unwrap(),
            "http://127.0.0.1:8008"
        );
        // Plain HTTP to a public host: refused before any request — the password
        // must not leave in cleartext.
        let err = accept_explicit_homeserver(&sn, "http://public-host").unwrap_err();
        assert!(err.to_string().contains("must use HTTPS"), "got: {err}");
    }

    #[test]
    fn loopback_hosts_are_recognized() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("example.org"));
        assert!(!is_loopback_host("192.0.2.7"));
    }
}
