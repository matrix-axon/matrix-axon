//! The `axon oauth` subcommand: bind upstream OIDC identities to this
//! instance's owner, and manage them (M14, ADR 0054's "CLI bind command").
//!
//! Like `token`, this connects only the [`Store`] — no sync engine, no HTTP
//! listener. Unlike `token`, `bind`'s browser leg depends on the long-running
//! `axon` server process already being up and reachable: the CLI only mints
//! the `oauth_bind_requests` row and polls it; `GET /v1/oauth/bind` (served
//! by that running process) is what actually drives the upstream redirect.

use std::time::Duration;

use anyhow::Context;
use axon_core::Config;
use axon_store::Store;
use chrono::{Duration as ChronoDuration, Utc};
use rand::Rng;

use crate::cli::{IdentitiesAction, OauthAction};

/// How long a bind handshake stays open before the browser leg must have
/// completed. Matches `axon_api::routes::oauth::AUTHORIZATION_REQUEST_TTL`'s
/// value (that constant is private to `axon-api`, so this is a separate copy,
/// not a shared one — both are "how long is a single-use OAuth flow allowed
/// to stay pending", not something that needs to be literally the same
/// constant to stay correct if one changes).
const BIND_REQUEST_TTL: ChronoDuration = ChronoDuration::minutes(10);

/// How often the CLI re-checks the bind request's status while waiting for
/// the browser leg to finish.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Unambiguous uppercase alphabet for `user_code` generation — excludes
/// characters humans commonly mistype or confuse (`0`/`O`, `1`/`I`), the same
/// concern RFC 8628 §6.1 flags for device-flow user codes.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Run an `oauth` subcommand against the configured database.
pub async fn run(action: OauthAction, config: &Config) -> anyhow::Result<()> {
    let store = Store::connect(&config.database.url, config.database.max_connections)
        .await
        .context("connecting to database")?;

    match action {
        OauthAction::Bind { provider } => bind(&store, config, &provider).await,
        OauthAction::Identities { action } => identities(&store, action).await,
    }
}

/// Start a bind handshake, print the URL for the admin to open, then poll
/// until it completes or expires. Validates `oauth.enabled` and the named
/// provider's own `enabled` flag up front — a clear local error beats
/// printing a URL that's guaranteed to 404.
async fn bind(store: &Store, config: &Config, provider: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.oauth.enabled,
        "oauth.enabled = false; enable oauth before binding an identity"
    );
    let provider_config = match provider {
        "google" => &config.oauth.providers.google,
        "microsoft" => &config.oauth.providers.microsoft,
        "apple" => {
            anyhow::bail!("Sign in with Apple callbacks are not integrated yet (ADR 0054) — it can't be bound")
        }
        other => anyhow::bail!("unknown provider {other:?} (expected \"google\" or \"microsoft\")"),
    };
    anyhow::ensure!(
        provider_config.enabled,
        "oauth.providers.{provider}.enabled = false; enable it before binding"
    );
    // Same presence check `main.rs`'s boot-time `discover_generic_provider`
    // runs before constructing the real provider — reused here (rather than
    // just checking `enabled`) so this can't pass for a config the running
    // server would actually refuse to serve.
    crate::require_generic_provider_configured(provider, provider_config)?;
    let external_base_url = config
        .oauth
        .external_base_url
        .as_deref()
        .filter(|url| !url.is_empty())
        .context("oauth.external_base_url must be set before binding")?;

    let user_code = generate_user_code();
    let expires_at = Utc::now() + BIND_REQUEST_TTL;
    let request = store
        .create_bind_request(provider, &user_code, expires_at)
        .await
        .context("creating bind request")?;

    println!(
        "Open {external_base_url}/v1/oauth/bind?user_code={user_code} in any browser and sign \
         in with {provider}.\nWaiting for sign-in to complete (expires in 10 minutes)..."
    );

    loop {
        let current = store
            .find_bind_request(request.device_code)
            .await
            .context("polling bind request")?
            .context("bind request row disappeared")?;
        match current.status.as_str() {
            "pending" if current.expires_at > Utc::now() => {
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            "completed" => {
                println!("Bound successfully.");
                return Ok(());
            }
            other => anyhow::bail!(
                "bind request did not complete (status {other:?}, likely expired) — try again"
            ),
        }
    }
}

/// List or unbind already-bound identities.
async fn identities(store: &Store, action: IdentitiesAction) -> anyhow::Result<()> {
    match action {
        IdentitiesAction::List => {
            let identities = store
                .list_identities()
                .await
                .context("listing identities")?;
            if identities.is_empty() {
                println!("No bound identities. Bind one with `axon oauth bind --provider <name>`.");
            } else {
                for identity in identities {
                    println!(
                        "{}  {:<10}  {:<40}  {:<30}  linked {}",
                        identity.id,
                        identity.provider,
                        identity.subject,
                        identity.email.as_deref().unwrap_or("(no email)"),
                        identity.linked_at.to_rfc3339(),
                    );
                }
            }
        }
        IdentitiesAction::Unbind { id } => {
            // Tokens/refresh tokens must be revoked before the identity row
            // can be deleted — the FK carries no `ON DELETE` action, by
            // design (see `Store::delete_identity`'s doc comment).
            let tokens_revoked = store
                .revoke_tokens_for_identity(id)
                .await
                .context("revoking tokens")?;
            let refresh_revoked = store
                .revoke_refresh_tokens_for_identity(id)
                .await
                .context("revoking refresh tokens")?;
            if store
                .delete_identity(id)
                .await
                .context("deleting identity")?
            {
                println!(
                    "Unbound identity {id} (revoked {tokens_revoked} token(s), \
                     {refresh_revoked} refresh token(s))."
                );
            } else {
                println!("No bound identity with id {id}.");
            }
        }
    }
    Ok(())
}

/// Generate an 8-character human-typeable code, `XXXX-XXXX` shaped, from
/// [`USER_CODE_ALPHABET`] — RFC 8628-style, not cryptographically load-bearing
/// on its own (the bind request's short TTL plus `/v1/oauth/*`'s per-`state`
/// rate limiting is what makes guessing impractical, same as Path A/B's
/// codes).
fn generate_user_code() -> String {
    let mut rng = rand::rng();
    let chars: String = (0..8)
        .map(|_| USER_CODE_ALPHABET[rng.random_range(0..USER_CODE_ALPHABET.len())] as char)
        .collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_code_is_eight_unambiguous_chars_grouped() {
        let code = generate_user_code();
        assert_eq!(code.len(), 9); // 8 chars + 1 separator
        assert_eq!(code.chars().nth(4), Some('-'));
        for c in code.chars().filter(|c| *c != '-') {
            assert!(
                USER_CODE_ALPHABET.contains(&(c as u8)),
                "{c} not in the unambiguous alphabet"
            );
        }
    }
}
