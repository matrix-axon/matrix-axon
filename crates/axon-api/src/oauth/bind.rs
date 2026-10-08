//! Starting an identity bind (ADR 0054 "CLI bind command", ADR 0109).
//!
//! Two callers start a bind: `axon oauth bind`, authorized by shell access,
//! and `POST /v1/management/oauth/binds`, authorized by the owner's bearer.
//! Both go through [`check`] and [`BindTarget::start`], so they agree on what
//! may be bound, on the code a person types, and on how long the handshake
//! stays open. The browser leg that follows (`GET /v1/oauth/bind`, the
//! upstream redirect, the callback) is the same for both.

use axon_store::{BindRequest, Store, StoreError};
use chrono::Utc;
use rand::Rng;

use super::{OAuthRuntime, HANDSHAKE_TTL};

/// The providers an identity can be bound with.
const PROVIDERS: [&str; 3] = ["apple", "google", "microsoft"];

/// Unambiguous uppercase alphabet for `user_code` generation — excludes
/// characters humans commonly mistype or confuse (`0`/`O`, `1`/`I`), the same
/// concern RFC 8628 §6.1 flags for device-flow user codes.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Why a bind cannot be started. The messages name configuration keys and
/// never their values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindRefusal {
    #[error("oauth.enabled = false; enable oauth before binding an identity")]
    OauthDisabled,
    #[error("unknown provider (expected apple, google, or microsoft)")]
    UnknownProvider,
    #[error("oauth.providers.{0}.enabled = false; enable it before binding")]
    ProviderDisabled(&'static str),
    #[error("{0}")]
    ProviderMisconfigured(String),
    #[error("oauth.external_base_url must be set before binding")]
    ExternalBaseUrlMissing,
}

/// How many binds may wait on a sign-in at once. Each is a code the
/// unauthenticated browser leg accepts for ten minutes, so this bounds how far
/// starting binds can improve a guesser's odds. One owner linking one identity
/// needs one; the slack is for attempts abandoned in the last few minutes.
const MAX_PENDING_BINDS: i64 = 5;

/// How many user codes to draw before giving up on a collision. One is
/// enough in practice: the space is 32^8 and the table holds a handful.
const USER_CODE_ATTEMPTS: u32 = 3;

/// Whether a start counts against [`MAX_PENDING_BINDS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingLimit {
    /// Refuse while too many binds are pending. For a start that a bearer
    /// authorized over the network.
    Capped,
    /// Always start. For the CLI: an operator with a shell is the way out
    /// when a client has left binds lying around, and must not have to wait
    /// ten minutes for them to lapse.
    Unlimited,
}

/// Why a bind that passed [`check`] was not started.
#[derive(Debug, thiserror::Error)]
pub enum StartBindError {
    #[error(
        "too many sign-in links are waiting to be used; finish one or wait for them to expire"
    )]
    TooManyPending,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A provider a bind may be started for, and where its browser leg is served.
/// Only [`check`] makes one, so holding it means the checks passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindTarget<'a> {
    provider: &'static str,
    external_base_url: &'a str,
}

/// A bind handshake that has just been created.
#[derive(Debug, Clone)]
pub struct StartedBind {
    /// The pending request. Its `device_code` is the id to poll.
    pub request: BindRequest,
    /// The URL the owner opens in a browser to sign in with the provider.
    pub url: String,
}

/// Decide whether a bind for `provider` may start: OAuth is enabled, the
/// provider is one Axon knows, it is ready, and there is an external base URL
/// to build the browser link from.
///
/// `provider_ready` is the one question the two callers answer differently.
/// The running server asks its constructed provider set, which is the truth
/// about what `GET /v1/oauth/bind` can redirect to. The CLI has no running
/// server to ask and reads the configuration instead. Everything else,
/// including the order the checks run in, is here.
pub fn check<'a>(
    oauth_enabled: bool,
    external_base_url: Option<&'a str>,
    provider: &str,
    provider_ready: impl FnOnce(&'static str) -> Result<(), BindRefusal>,
) -> Result<BindTarget<'a>, BindRefusal> {
    if !oauth_enabled {
        return Err(BindRefusal::OauthDisabled);
    }
    let provider = PROVIDERS
        .into_iter()
        .find(|known| *known == provider)
        .ok_or(BindRefusal::UnknownProvider)?;
    provider_ready(provider)?;
    let external_base_url = external_base_url
        .map(|url| url.trim_end_matches('/'))
        .filter(|url| !url.is_empty())
        .ok_or(BindRefusal::ExternalBaseUrlMissing)?;
    Ok(BindTarget {
        provider,
        external_base_url,
    })
}

impl OAuthRuntime {
    /// [`check`] against the running server: a provider is ready when it was
    /// constructed at boot, which means enabled and fully configured. Apple
    /// with only its native flow on has no browser provider and is refused.
    /// `None` is `oauth.enabled = false`.
    pub fn bind_target<'a>(
        runtime: Option<&'a OAuthRuntime>,
        provider: &str,
    ) -> Result<BindTarget<'a>, BindRefusal> {
        check(
            runtime.is_some(),
            runtime.map(|runtime| runtime.external_base_url.as_str()),
            provider,
            |name| match runtime.and_then(|runtime| runtime.provider(name)) {
                Some(_) => Ok(()),
                None => Err(BindRefusal::ProviderDisabled(name)),
            },
        )
    }
}

impl BindTarget<'_> {
    /// The provider's name.
    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// Create the pending bind request and the URL that starts its browser
    /// leg. The request expires [`HANDSHAKE_TTL`] from now.
    ///
    /// With [`PendingLimit::Capped`] it is refused with
    /// [`StartBindError::TooManyPending`] while [`MAX_PENDING_BINDS`] are
    /// already waiting.
    pub async fn start(
        &self,
        store: &Store,
        limit: PendingLimit,
    ) -> Result<StartedBind, StartBindError> {
        let mut attempt = 0;
        let request = loop {
            attempt += 1;
            let user_code = generate_user_code();
            let expires_at = Utc::now() + HANDSHAKE_TTL;
            let created = match limit {
                PendingLimit::Capped => store
                    .create_bind_request_unless_too_many(
                        self.provider,
                        &user_code,
                        expires_at,
                        MAX_PENDING_BINDS,
                    )
                    .await
                    .map(|request| request.ok_or(StartBindError::TooManyPending)),
                PendingLimit::Unlimited => store
                    .create_bind_request(self.provider, &user_code, expires_at)
                    .await
                    .map(Ok),
            };
            match created {
                Ok(request) => break request?,
                // The code is taken, by a pending bind or a completed one
                // still on record. Another draw is all it needs.
                Err(error) if error.is_unique_violation() && attempt < USER_CODE_ATTEMPTS => {}
                Err(error) => return Err(error.into()),
            }
        };
        let url = format!(
            "{}/v1/oauth/bind?user_code={}",
            self.external_base_url, request.user_code
        );
        Ok(StartedBind { request, url })
    }
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

    fn ready(_: &'static str) -> Result<(), BindRefusal> {
        Ok(())
    }

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

    #[test]
    fn a_ready_known_provider_with_a_base_url_may_bind() {
        let target = check(true, Some("https://axon.example/"), "google", ready).unwrap();
        assert_eq!(target.provider(), "google");
        assert_eq!(target.external_base_url, "https://axon.example");
    }

    #[test]
    fn each_missing_precondition_is_refused_by_name() {
        assert_eq!(
            check(false, Some("https://axon.example"), "google", ready),
            Err(BindRefusal::OauthDisabled)
        );
        assert_eq!(
            check(true, Some("https://axon.example"), "github", ready),
            Err(BindRefusal::UnknownProvider)
        );
        assert_eq!(
            check(true, Some("https://axon.example"), "apple", |name| Err(
                BindRefusal::ProviderDisabled(name)
            )),
            Err(BindRefusal::ProviderDisabled("apple"))
        );
        for base in [None, Some(""), Some("/")] {
            assert_eq!(
                check(true, base, "google", ready),
                Err(BindRefusal::ExternalBaseUrlMissing)
            );
        }
    }

    #[test]
    fn an_unknown_provider_never_reaches_the_readiness_question() {
        // The closure receives only names from the known list, so a caller's
        // lookup cannot be steered by whatever string the request carried.
        let refusal = check(true, Some("https://axon.example"), "Google", |_| {
            panic!("asked about a provider that is not on the list")
        });
        assert_eq!(refusal, Err(BindRefusal::UnknownProvider));
    }

    #[test]
    fn the_running_server_refuses_what_it_did_not_construct() {
        assert_eq!(
            OAuthRuntime::bind_target(None, "google"),
            Err(BindRefusal::OauthDisabled)
        );
        let config = axon_core::OauthConfig {
            enabled: true,
            external_base_url: Some("https://axon.example".into()),
            ..Default::default()
        };
        let runtime = OAuthRuntime::new(&config, std::collections::HashMap::new());
        assert_eq!(
            OAuthRuntime::bind_target(Some(&runtime), "google"),
            Err(BindRefusal::ProviderDisabled("google"))
        );
    }
}
