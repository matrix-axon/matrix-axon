//! Shared, bounded authorization-code exchange for upstream providers.

use serde::Deserialize;

use super::{read_json_capped, OidcError, UpstreamTokens, MAX_HTTP_RESPONSE_BYTES};

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

pub(super) async fn authorization_code(
    http: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<UpstreamTokens, OidcError> {
    let response = http
        .post(token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ])
        .send()
        .await
        .map_err(|err| super::transport_error("OIDC token request", err))?;
    if !response.status().is_success() {
        // Provider error bodies and request payloads can carry credentials.
        return Err(OidcError::Http(format!(
            "token exchange returned {}",
            response.status()
        )));
    }
    let body: TokenResponse = read_json_capped(response, MAX_HTTP_RESPONSE_BYTES)
        .await
        .map_err(|error| match error {
            error @ OidcError::Http(_) => error,
            _ => OidcError::Malformed("invalid OIDC token response".into()),
        })?;
    if body.id_token.is_empty() {
        return Err(OidcError::Malformed("missing identity token".into()));
    }
    Ok(UpstreamTokens {
        id_token: body.id_token,
    })
}
