//! Load `[push]` once at startup (ADR 0113).
//!
//! A provider that is empty is not configured. A provider that is only partly
//! set, or whose key file cannot be read, stops startup. The error does not
//! contain the PEM. Sends, in a later change, use the bytes returned here and
//! do not read the file again.

use anyhow::Context;
use axon_core::{LoadedApns, LoadedFcm, PushConfig, PushKeySource, PushMaterial};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

use crate::pem::load_private_key;

/// Resolve `[push]` and read each configured PEM.
pub(crate) async fn load_push_material(config: &PushConfig) -> anyhow::Result<PushMaterial> {
    let apns = match config.apns.resolve().context("reading push.apns")? {
        None => None,
        Some(resolved) => {
            let private_key = load_key("APNs", &resolved.key).await?;
            parse_signing_key("APNs", Algorithm::ES256, &private_key)?;
            tracing::info!(
                team_id = %resolved.team_id,
                key_id = %resolved.key_id,
                bundle_id = %resolved.bundle_id,
                key_bytes = private_key.len(),
                "APNs push key loaded"
            );
            Some(LoadedApns {
                team_id: resolved.team_id,
                key_id: resolved.key_id,
                bundle_id: resolved.bundle_id,
                private_key,
            })
        }
    };
    let fcm = match config.fcm.resolve().context("reading push.fcm")? {
        None => None,
        Some(resolved) => {
            let private_key = load_key("FCM", &resolved.key).await?;
            parse_signing_key("FCM", Algorithm::RS256, &private_key)?;
            tracing::info!(
                project_id = %resolved.project_id,
                package_name = %resolved.package_name,
                key_bytes = private_key.len(),
                "FCM push key loaded"
            );
            Some(LoadedFcm {
                project_id: resolved.project_id,
                client_email: resolved.client_email,
                package_name: resolved.package_name,
                private_key,
            })
        }
    };
    if apns.is_none() {
        tracing::info!("APNs push is not configured");
    }
    if fcm.is_none() {
        tracing::info!("FCM push is not configured");
    }
    Ok(PushMaterial::new(apns, fcm))
}

async fn load_key(label: &str, source: &PushKeySource) -> anyhow::Result<Vec<u8>> {
    load_private_key(label, source).await
}

/// Prove the PEM is a usable signing key of `algorithm` before status can say
/// the provider is configured. The parser error is dropped: it can echo key
/// material. A thrown-away signature is enough to reject a certificate, the
/// wrong algorithm, or an env value whose newlines are still the two
/// characters `\` and `n`.
fn parse_signing_key(label: &str, algorithm: Algorithm, pem: &[u8]) -> anyhow::Result<()> {
    let key = match algorithm {
        Algorithm::ES256 => EncodingKey::from_ec_pem(pem),
        Algorithm::RS256 => EncodingKey::from_rsa_pem(pem),
        _ => anyhow::bail!("invalid {label} private key"),
    };
    let key = key.map_err(|_| anyhow::anyhow!("invalid {label} private key"))?;
    encode(&Header::new(algorithm), &ProbeClaims { probe: true }, &key)
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("invalid {label} private key"))
}

#[derive(serde::Serialize)]
struct ProbeClaims {
    probe: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_core::{ApnsPushConfig, FcmPushConfig};

    fn configured_apns(key: &str) -> PushConfig {
        PushConfig {
            apns: ApnsPushConfig {
                team_id: Some("TEAM".into()),
                key_id: Some("KEYID".into()),
                bundle_id: Some("org.matrixaxon.axon".into()),
                private_key: Some(key.into()),
                ..ApnsPushConfig::default()
            },
            ..PushConfig::default()
        }
    }

    async fn expect_loaded(config: &PushConfig) -> PushMaterial {
        match load_push_material(config).await {
            Ok(material) => material,
            Err(error) => panic!("push config failed to load: {error}"),
        }
    }

    #[tokio::test]
    async fn an_empty_section_loads_nothing() {
        let material = expect_loaded(&PushConfig::default()).await;
        assert!(!material.apns_configured());
        assert!(!material.fcm_configured());
    }

    #[tokio::test]
    async fn a_partial_provider_fails_without_the_pem() {
        let config = PushConfig {
            fcm: FcmPushConfig {
                project_id: Some("proj".into()),
                private_key: Some("PRIVATE_SENTINEL".into()),
                ..FcmPushConfig::default()
            },
            ..PushConfig::default()
        };
        let error = match load_push_material(&config).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("partial push config was accepted"),
        };
        assert!(error.contains("push.fcm"));
        assert!(!error.contains("PRIVATE_SENTINEL"));
    }

    #[tokio::test]
    async fn an_inline_key_is_held_and_not_reread() {
        let pem = axon_test_support::ec_key().pem.clone();
        let material = expect_loaded(&configured_apns(&pem)).await;
        let Some(apns) = material.apns() else {
            panic!("apns key was not loaded");
        };
        assert_eq!(apns.private_key, pem.as_bytes());
        assert!(material.apns_configured());
        assert!(!material.fcm_configured());
    }

    #[tokio::test]
    async fn a_malformed_or_wrong_key_fails_without_echoing_it() {
        let pem = axon_test_support::ec_key().pem.clone();
        let marker: String = pem.chars().skip(40).take(24).collect();
        let cases = [
            configured_apns("PRIVATE_SENTINEL"),
            configured_apns(&pem.replace('\n', "\\n")),
            configured_apns(axon_test_support::rsa_key_pem()),
        ];
        for config in cases {
            let error = match load_push_material(&config).await {
                Err(error) => error.to_string(),
                Ok(_) => panic!("malformed APNs key was accepted"),
            };
            assert!(error.contains("invalid APNs"), "{error}");
            assert!(!error.contains("PRIVATE_SENTINEL"), "{error}");
            assert!(!error.contains(&marker), "{error}");
        }

        let fcm = PushConfig {
            fcm: FcmPushConfig {
                project_id: Some("proj".into()),
                client_email: Some("svc@example.com".into()),
                package_name: Some("org.matrixaxon.axon".into()),
                private_key: Some(pem),
                ..FcmPushConfig::default()
            },
            ..PushConfig::default()
        };
        let error = match load_push_material(&fcm).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("an EC key was accepted as FCM"),
        };
        assert!(error.contains("invalid FCM"), "{error}");
        assert!(!error.contains(&marker), "{error}");
    }

    #[tokio::test]
    async fn a_key_file_must_be_owner_only_and_the_error_omits_the_path_name() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("PRIVATE_SENTINEL");
        let pem = axon_test_support::ec_key().pem.clone();
        std::fs::write(&path, pem.as_bytes()).unwrap();
        let mut config = configured_apns("");
        config.apns.private_key = None;
        config.apns.private_key_path = Some(path.clone());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let error = match load_push_material(&config).await {
                Err(error) => error.to_string(),
                Ok(_) => panic!("a world-readable key file was accepted"),
            };
            assert!(error.contains("owner-only"));
            assert!(!error.contains("PRIVATE_SENTINEL"));
            assert!(!error.contains("BEGIN PRIVATE KEY"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let material = expect_loaded(&config).await;
        let Some(apns) = material.apns() else {
            panic!("apns key file was not loaded");
        };
        assert_eq!(apns.private_key, pem.as_bytes());
    }
}
