//! Shared signature and claim verification for upstream OIDC providers.

use super::{jwks::JwksCache, OidcError, VerifiedIdentity};
use chrono::Utc;
use jsonwebtoken::{decode, decode_header, Validation};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(super) const CLOCK_SKEW_SECS: i64 = 60;

#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    #[serde(default)]
    aud: Value,
    sub: String,
    exp: i64,
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    jti: Option<String>,
    /// Microsoft's tenant id claim — substituted into a `{tenantid}`-templated
    /// issuer to validate multi-tenant tokens. Absent for Google.
    #[serde(default)]
    tid: Option<String>,
}

/// Verify against the caller's explicit issuer, audiences, and expected nonce.
pub(super) async fn verify(
    jwks: &JwksCache,
    token: &str,
    issuer: &str,
    audiences: &[&str],
    nonce: Option<&str>,
) -> Result<VerifiedIdentity, OidcError> {
    if token.len() > 16 * 1024 {
        return Err(OidcError::Malformed(
            "identity token exceeds size limit".into(),
        ));
    }
    let header =
        decode_header(token).map_err(|_| OidcError::Malformed("invalid token header".into()))?;
    let kid = header
        .kid
        .ok_or_else(|| OidcError::BadSignature("token header has no kid".to_owned()))?;
    let (algorithm, decoding_key) = jwks.resolve(&kid).await.map_err(|error| match error {
        // JWKS transport errors are sanitized at their source and retain only
        // fixed categories or an HTTP status, never a URL or response body.
        error @ OidcError::Http(_) => error,
        OidcError::DisallowedAlgorithm(_) => {
            OidcError::DisallowedAlgorithm("unsupported signing key".into())
        }
        _ => OidcError::BadSignature("signing key unavailable".into()),
    })?;
    if header.alg != algorithm {
        return Err(OidcError::DisallowedAlgorithm(
            "token and key algorithms differ".into(),
        ));
    }

    // Signature/structure verification is jsonwebtoken's job; iss/aud/
    // nonce/exp/nbf/iat are checked by hand below so the Microsoft
    // template case has an explicit, testable seam.
    let mut validation = Validation::new(algorithm);
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    let data = decode::<Claims>(token, &decoding_key, &validation)
        .map_err(|_| OidcError::BadSignature("invalid signed identity token".into()))?;
    let claims = data.claims;

    if !issuer_matches(issuer, &claims.iss, claims.tid.as_deref()) {
        return Err(OidcError::InvalidIssuer("issuer mismatch".into()));
    }
    if !audiences
        .iter()
        .any(|audience| audience_contains(&claims.aud, audience))
    {
        return Err(OidcError::InvalidAudience("audience mismatch".into()));
    }
    if let Some(expected_nonce) = nonce {
        if claims.nonce.as_deref() != Some(expected_nonce) {
            return Err(OidcError::InvalidNonce);
        }
    }

    let now = Utc::now().timestamp();
    if claims.exp.saturating_add(CLOCK_SKEW_SECS) < now {
        return Err(OidcError::Expired(format!(
            "token expired at {}",
            claims.exp
        )));
    }
    if let Some(nbf) = claims.nbf {
        if nbf.saturating_sub(CLOCK_SKEW_SECS) > now {
            return Err(OidcError::Expired("token not yet valid".to_owned()));
        }
    }
    if claims.iat.saturating_sub(CLOCK_SKEW_SECS) > now {
        return Err(OidcError::Expired("token issued in the future".to_owned()));
    }

    if claims.sub.is_empty() {
        return Err(OidcError::Malformed("empty identity subject".into()));
    }
    let replay_key = claims
        .jti
        .filter(|jti| !jti.is_empty())
        .unwrap_or_else(|| hash_token(token));

    Ok(VerifiedIdentity {
        subject: claims.sub,
        email: claims.email,
        replay_key,
    })
}

/// True if `token_iss` satisfies `configured_issuer`. Handles Microsoft's
/// multi-tenant discovery issuer, which is a **template** containing the
/// literal string `{tenantid}` rather than a concrete value: the token's own
/// `tid` claim is substituted in before comparing. Any other provider's
/// issuer (no `{tenantid}` placeholder) is compared for exact equality, as
/// normal.
pub(crate) fn issuer_matches(
    configured_issuer: &str,
    token_iss: &str,
    token_tid: Option<&str>,
) -> bool {
    if configured_issuer.contains("{tenantid}") {
        return match token_tid {
            Some(tid) => configured_issuer.replace("{tenantid}", tid) == token_iss,
            None => false,
        };
    }
    configured_issuer == token_iss
}

/// True if `aud` (a JWT `aud` claim: either a bare string or an array of
/// strings) contains `expected`.
pub(super) fn audience_contains(aud: &Value, expected: &str) -> bool {
    match aud {
        Value::String(s) => s == expected,
        Value::Array(values) => values.iter().any(|v| v.as_str() == Some(expected)),
        _ => false,
    }
}

/// Fallback replay key when a provider omits `jti`: a hash of the raw token.
fn hash_token(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, digest)
}
