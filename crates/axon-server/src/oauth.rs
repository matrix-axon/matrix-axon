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
use axon_api::oauth_bind::{self, BindRefusal};
use axon_core::Config;
use axon_store::Store;
use chrono::Utc;

use crate::cli::{IdentitiesAction, OauthAction};

/// How often the CLI re-checks the bind request's status while waiting for
/// the browser leg to finish.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

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

/// Whether `provider` is enabled and fully configured, read from the config
/// file: the CLI's answer to the one question [`oauth_bind::check`] leaves to
/// its caller. The running server answers it from the providers it built at
/// boot, by the same [`crate::require_generic_provider_configured`].
fn provider_configured(
    config: &axon_core::OauthConfig,
    provider: &'static str,
) -> Result<(), BindRefusal> {
    let generic = match provider {
        "apple" => {
            return if config.providers.apple.enabled {
                Ok(())
            } else {
                Err(BindRefusal::ProviderDisabled(provider))
            };
        }
        "google" => &config.providers.google,
        _ => &config.providers.microsoft,
    };
    if !generic.enabled {
        return Err(BindRefusal::ProviderDisabled(provider));
    }
    crate::require_generic_provider_configured(provider, generic)
        .map(|_| ())
        .map_err(|error| BindRefusal::ProviderMisconfigured(error.to_string()))
}

/// Start a bind handshake, print the URL for the admin to open, then poll
/// until it completes or expires. The checks and the handshake itself are
/// the ones the management API's bind runs ([`oauth_bind`]) — a clear local
/// error beats printing a URL that's guaranteed to 404.
async fn bind(store: &Store, config: &Config, provider: &str) -> anyhow::Result<()> {
    let oauth = &config.oauth;
    let target = oauth_bind::check(
        oauth.enabled,
        oauth.external_base_url.as_deref(),
        provider,
        |provider| provider_configured(oauth, provider),
    )?;
    if target.provider() == "apple" {
        // Apple's key has to load too, which no config field alone shows.
        apple_provider(oauth).await?;
    }

    let started = target.start(store).await.context("creating bind request")?;
    let request = started.request;
    println!(
        "Open {} in any browser and sign in with {}.\nWaiting for sign-in to complete (expires in {} minutes)...",
        started.url,
        target.provider(),
        axon_api::OAUTH_HANDSHAKE_TTL.num_minutes(),
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
                "bind request did not complete (canceled, failed, or expired; status {other:?}) — run oauth bind again to start a new attempt"
            ),
        }
    }
}

/// Shared startup/CLI validation. Never attach an IO error or path to these
/// diagnostics: operator-supplied configuration may itself contain secrets.
pub(crate) async fn apple_provider(
    config: &axon_core::OauthConfig,
) -> anyhow::Result<axon_api::AppleProvider> {
    apple_provider_with_http(config, axon_api::oauth_http_client()).await
}

pub(crate) async fn apple_provider_with_http(
    config: &axon_core::OauthConfig,
    http: reqwest::Client,
) -> anyhow::Result<axon_api::AppleProvider> {
    use tokio::io::AsyncReadExt;
    const MAX_KEY_BYTES: usize = 16 * 1024;
    let apple = &config.providers.apple;
    let callback = axon_api::oauth_callback_url(
        config.external_base_url.as_deref().unwrap_or_default(),
        "apple",
    );
    let url =
        url::Url::parse(&callback).map_err(|_| anyhow::anyhow!("invalid Apple callback URL"))?;
    anyhow::ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.as_str() == callback,
        "Apple callback requires a canonical HTTPS URL without userinfo, query, or fragment"
    );
    anyhow::ensure!(
        apple
            .redirect_uri
            .as_ref()
            .is_none_or(|explicit| explicit == &callback),
        "Apple redirect_uri must exactly match external_base_url plus /v1/oauth/apple/callback"
    );
    let pem = match (&apple.private_key, &apple.private_key_path) {
        (Some(pem), None) if !pem.is_empty() && pem.len() <= MAX_KEY_BYTES => {
            pem.as_bytes().to_vec()
        }
        (None, Some(path)) => tokio::time::timeout(Duration::from_secs(5), async {
            let metadata = tokio::fs::metadata(path)
                .await
                .map_err(|error| key_file_error("inspect", path.is_relative(), error.kind()))?;
            anyhow::ensure!(
                metadata.is_file() && metadata.len() <= MAX_KEY_BYTES as u64,
                "Apple key must be a regular file of at most 16 KiB"
            );
            let mut options = tokio::fs::OpenOptions::new();
            options.read(true);
            // A path swapped to a FIFO after inspection must not strand a
            // blocking filesystem worker even after the async timeout fires.
            #[cfg(unix)]
            options.custom_flags(libc::O_NONBLOCK);
            let file = options
                .open(path)
                .await
                .map_err(|error| key_file_error("read", path.is_relative(), error.kind()))?;
            let metadata = file
                .metadata()
                .await
                .map_err(|error| key_file_error("inspect", path.is_relative(), error.kind()))?;
            anyhow::ensure!(metadata.is_file(), "Apple key must be a regular file");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                anyhow::ensure!(
                    metadata.permissions().mode() & 0o077 == 0,
                    "Apple key file must have owner-only permissions (chmod 600)"
                );
            }
            let mut bytes = Vec::new();
            file.take((MAX_KEY_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| key_file_error("read", path.is_relative(), error.kind()))?;
            anyhow::ensure!(
                !bytes.is_empty() && bytes.len() <= MAX_KEY_BYTES,
                "Apple key file must contain at most 16 KiB of PEM"
            );
            Ok::<_, anyhow::Error>(bytes)
        })
        .await
        .map_err(|_| anyhow::anyhow!("Apple key file read timed out"))??,
        _ => anyhow::bail!(
            "set exactly one of Apple private_key (at most 16 KiB) or private_key_path"
        ),
    };
    axon_api::AppleProvider::new(http, apple, &pem)
        .map_err(|_| anyhow::anyhow!("invalid Apple provider credentials"))
}

fn key_file_error(
    operation: &'static str,
    relative: bool,
    kind: std::io::ErrorKind,
) -> anyhow::Error {
    let hint = if relative {
        "private_key_path is relative to the process working directory; use an absolute path"
    } else {
        "check private_key_path and file permissions"
    };
    anyhow::anyhow!("cannot {operation} Apple key file ({kind:?}); {hint}")
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
            if store
                .delete_identity(id)
                .await
                .context("deleting identity")?
            {
                println!(
                    "Unbound identity {id} (invalidated associated access tokens, \
                     refresh tokens, and authorization codes)."
                );
            } else {
                println!("No bound identity with id {id}.");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_test_support::{ec_key, TEST_KID};

    #[tokio::test]
    async fn apple_startup_registers_browser_provider_with_a_valid_callback() {
        let mut config = axon_core::OauthConfig {
            enabled: true,
            external_base_url: Some("https://axon.example/prefix/".into()),
            ..Default::default()
        };
        config.providers.apple = axon_core::AppleOauthConfig {
            enabled: true,
            client_id: Some("com.example.web".into()),
            team_id: Some("TEAM".into()),
            key_id: Some(TEST_KID.into()),
            private_key: Some(ec_key().pem.clone()),
            redirect_uri: Some("https://axon.example/prefix/v1/oauth/apple/callback".into()),
            ..Default::default()
        };
        let runtime = crate::build_oauth_runtime(&config).await.unwrap();
        let provider = runtime.provider("apple").unwrap();
        let callback = runtime.callback_url("apple");
        let state = uuid::Uuid::new_v4().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        let url = url::Url::parse(&provider.authorize_url(&state, &nonce, &callback)).unwrap();
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "redirect_uri" && value == callback));
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "response_mode" && value == "form_post"));
        assert!(provider
            .verify_identity_token("unused", None)
            .await
            .is_err());
        config.providers.apple.enabled = false;
        assert!(crate::build_oauth_runtime(&config)
            .await
            .unwrap()
            .provider("apple")
            .is_none());
    }

    #[tokio::test]
    async fn apple_key_file_checks_size_permissions_and_redacts_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("PRIVATE_SENTINEL");
        // Not a key: no private signing material is ever persisted by tests.
        std::fs::write(&path, "NOT_A_KEY_SENTINEL").unwrap();
        let mut config = axon_core::OauthConfig {
            external_base_url: Some("https://axon.example".into()),
            ..Default::default()
        };
        config.providers.apple.private_key_path = Some(path.clone());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(apple_provider(&config)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("owner-only"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            apple_provider(&config).await.err().unwrap().to_string(),
            "invalid Apple provider credentials"
        );
        std::fs::write(&path, vec![b'x'; 16385]).unwrap();
        assert!(apple_provider(&config)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("at most 16 KiB"));
    }

    #[tokio::test]
    async fn apple_callback_and_key_sources_fail_closed_without_disclosing_inputs() {
        let mut config = axon_core::OauthConfig {
            external_base_url: Some("https://axon.example".into()),
            ..Default::default()
        };
        config.providers.apple.private_key = Some("PRIVATE_SENTINEL".into());
        for base in [
            "http://axon.example",
            "https://user:PRIVATE_SENTINEL@axon.example",
            "https://axon.example?PRIVATE_SENTINEL",
            "https://axon.example#PRIVATE_SENTINEL",
        ] {
            config.external_base_url = Some(base.into());
            let error = apple_provider(&config).await.err().unwrap().to_string();
            assert!(error.contains("callback"));
            assert!(!error.contains("PRIVATE_SENTINEL"));
        }
        config.external_base_url = Some("https://axon.example".into());
        config.providers.apple.redirect_uri = Some("https://other.example/PRIVATE_SENTINEL".into());
        assert!(apple_provider(&config)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("exactly match"));
        config.providers.apple.redirect_uri = None;
        config.providers.apple.private_key_path = Some("PRIVATE_SENTINEL".into());
        assert!(apple_provider(&config)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("exactly one"));
        config.providers.apple.private_key = None;
        let error = apple_provider(&config).await.err().unwrap().to_string();
        assert!(error.contains("NotFound"));
        assert!(error.contains("relative to the process working directory"));
        assert!(!error.contains("PRIVATE_SENTINEL"));
        config.providers.apple.private_key_path = Some(std::env::temp_dir());
        assert!(apple_provider(&config)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("regular file"));
        config.providers.apple.private_key_path = None;
        config.providers.apple.private_key = Some("x".repeat(16385));
        assert!(apple_provider(&config)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("16 KiB"));
    }
}
