//! Remote-push provider configuration (ADR 0113).
//!
//! An absent section, or a provider whose every field is empty, is not
//! configured and is not an error. A provider that is only partly set fails
//! startup. The private key is either inline PEM or a path, never both, and
//! [`crate::Config`]'s `Debug` redacts both.

use std::path::PathBuf;

use serde::Deserialize;

use crate::error::ConfigError;

/// `[push]` — APNs and FCM key material. Delivery is a later change. This
/// section only decides whether each provider's key can be loaded.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PushConfig {
    /// Apple Push Notification service, for iOS and iPadOS.
    #[serde(default)]
    pub apns: ApnsPushConfig,
    /// Firebase Cloud Messaging, for Android.
    #[serde(default)]
    pub fcm: FcmPushConfig,
}

/// `[push.apns]`.
#[derive(Clone, Default, Deserialize)]
pub struct ApnsPushConfig {
    /// Apple Developer team id.
    #[serde(default)]
    pub team_id: Option<String>,
    /// APNs auth key id.
    #[serde(default)]
    pub key_id: Option<String>,
    /// PEM of the APNs auth key (.p8). Mutually exclusive with
    /// [`Self::private_key_path`].
    #[serde(default)]
    pub private_key: Option<String>,
    /// Path of the APNs auth key. Mutually exclusive with [`Self::private_key`].
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    /// App bundle id, sent as `apns-topic`.
    #[serde(default)]
    pub bundle_id: Option<String>,
}

/// `[push.fcm]`.
#[derive(Clone, Default, Deserialize)]
pub struct FcmPushConfig {
    /// Firebase project id.
    #[serde(default)]
    pub project_id: Option<String>,
    /// Service-account client email.
    #[serde(default)]
    pub client_email: Option<String>,
    /// PEM of the service-account private key. Mutually exclusive with
    /// [`Self::private_key_path`].
    #[serde(default)]
    pub private_key: Option<String>,
    /// Path of the service-account private key. Mutually exclusive with
    /// [`Self::private_key`].
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    /// Android package name the registration tokens belong to.
    #[serde(default)]
    pub package_name: Option<String>,
}

macro_rules! debug_provider {
    ($ty:ident { $($field:ident),+ $(,)? }) => {
        impl std::fmt::Debug for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($field), &self.$field))+
                    .field(
                        "private_key",
                        &crate::config::redacted_option(&self.private_key),
                    )
                    .field(
                        "private_key_path",
                        &crate::config::redacted_option(&self.private_key_path),
                    )
                    .finish()
            }
        }
    };
}

debug_provider!(ApnsPushConfig {
    team_id,
    key_id,
    bundle_id
});
debug_provider!(FcmPushConfig {
    project_id,
    client_email,
    package_name
});

impl PushConfig {
    /// Reject a provider that is only partly set. An empty provider is fine.
    /// This does not read key files; that happens when the process starts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut problems = Vec::new();
        if let Err(error) = self.apns.resolve() {
            problems.push(validation_text(error));
        }
        if let Err(error) = self.fcm.resolve() {
            problems.push(validation_text(error));
        }
        match problems.len() {
            0 => Ok(()),
            _ => Err(ConfigError::Validation(problems.join("; "))),
        }
    }
}

/// Where a configured provider's PEM comes from. Exactly one of the two.
#[derive(Clone)]
pub enum PushKeySource {
    /// Inline PEM from the config file or environment.
    Inline(String),
    /// A path the process will read once at startup.
    Path(PathBuf),
}

/// APNs settings after the "empty or complete" check. The PEM is not loaded yet.
#[derive(Clone)]
pub struct ResolvedApns {
    pub team_id: String,
    pub key_id: String,
    pub bundle_id: String,
    pub key: PushKeySource,
}

/// FCM settings after the "empty or complete" check. The PEM is not loaded yet.
#[derive(Clone)]
pub struct ResolvedFcm {
    pub project_id: String,
    pub client_email: String,
    pub package_name: String,
    pub key: PushKeySource,
}

impl ApnsPushConfig {
    /// `Ok(None)` when every field is empty.
    /// `Ok(Some)` when team id, key id, bundle id, and exactly one key source
    /// are set. Anything in between, including both key sources, is an error.
    pub fn resolve(&self) -> Result<Option<ResolvedApns>, ConfigError> {
        let Some((team_id, key_id, bundle_id, key)) = resolve_provider(
            "push.apns",
            "team_id, key_id, bundle_id",
            filled(self.team_id.as_deref()),
            filled(self.key_id.as_deref()),
            filled(self.bundle_id.as_deref()),
            self.private_key.as_deref(),
            self.private_key_path.as_ref(),
        )?
        else {
            return Ok(None);
        };
        Ok(Some(ResolvedApns {
            team_id,
            key_id,
            bundle_id,
            key,
        }))
    }
}

impl FcmPushConfig {
    /// `Ok(None)` when every field is empty.
    /// `Ok(Some)` when project id, client email, package name, and exactly one
    /// key source are set. Anything in between, including both key sources, is
    /// an error.
    pub fn resolve(&self) -> Result<Option<ResolvedFcm>, ConfigError> {
        let Some((project_id, client_email, package_name, key)) = resolve_provider(
            "push.fcm",
            "project_id, client_email, package_name",
            filled(self.project_id.as_deref()),
            filled(self.client_email.as_deref()),
            filled(self.package_name.as_deref()),
            self.private_key.as_deref(),
            self.private_key_path.as_ref(),
        )?
        else {
            return Ok(None);
        };
        Ok(Some(ResolvedFcm {
            project_id,
            client_email,
            package_name,
            key,
        }))
    }
}

/// Provider keys this process loaded. Absent means that provider is not
/// configured. There is no `Debug`: the PEM must not land in a log line.
#[derive(Clone, Default)]
pub struct PushMaterial {
    apns: Option<LoadedApns>,
    fcm: Option<LoadedFcm>,
}

/// APNs auth key bytes plus the ids a later send needs.
#[derive(Clone)]
pub struct LoadedApns {
    pub team_id: String,
    pub key_id: String,
    pub bundle_id: String,
    pub private_key: Vec<u8>,
}

/// FCM service-account key bytes plus the ids a later send needs.
#[derive(Clone)]
pub struct LoadedFcm {
    pub project_id: String,
    pub client_email: String,
    pub package_name: String,
    pub private_key: Vec<u8>,
}

impl PushMaterial {
    /// Neither provider loaded.
    pub fn unconfigured() -> Self {
        Self::default()
    }

    /// Keep the keys that startup loaded. `None` is an unconfigured provider.
    pub fn new(apns: Option<LoadedApns>, fcm: Option<LoadedFcm>) -> Self {
        Self { apns, fcm }
    }

    /// Whether APNs key bytes are in memory.
    pub fn apns_configured(&self) -> bool {
        self.apns.is_some()
    }

    /// Whether FCM key bytes are in memory.
    pub fn fcm_configured(&self) -> bool {
        self.fcm.is_some()
    }

    /// The loaded APNs key, when this process has one.
    pub fn apns(&self) -> Option<&LoadedApns> {
        self.apns.as_ref()
    }

    /// The loaded FCM key, when this process has one.
    pub fn fcm(&self) -> Option<&LoadedFcm> {
        self.fcm.as_ref()
    }
}

fn validation_text(error: ConfigError) -> String {
    match error {
        ConfigError::Validation(message) => message,
        other => other.to_string(),
    }
}

/// Trimmed text, or `None` when the value is missing or only whitespace.
fn filled(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn resolve_provider(
    which: &str,
    field_list: &str,
    first: Option<String>,
    second: Option<String>,
    third: Option<String>,
    inline: Option<&str>,
    path: Option<&PathBuf>,
) -> Result<Option<(String, String, String, PushKeySource)>, ConfigError> {
    match (first, second, third, key_source(inline, path)) {
        (None, None, None, KeyPresence::Absent) => Ok(None),
        (Some(first), Some(second), Some(third), KeyPresence::One(key)) => {
            Ok(Some((first, second, third, key)))
        }
        _ => Err(ConfigError::Validation(format!(
            "{which} is partly set; set {field_list}, and exactly one of private_key or \
             private_key_path, or leave every field empty"
        ))),
    }
}

enum KeyPresence {
    Absent,
    One(PushKeySource),
    Both,
}

/// Neither source, exactly one, or both. Both is its own case so a provider
/// that sets the two key fields and nothing else is still an error.
fn key_source(inline: Option<&str>, path: Option<&PathBuf>) -> KeyPresence {
    // Trim only to decide emptiness. The stored PEM keeps a real key's
    // surrounding newline; a whitespace-only value is absent.
    let inline = inline
        .filter(|pem| !pem.trim().is_empty())
        .map(str::to_owned);
    let path = path.filter(|path| {
        path.as_os_str()
            .to_str()
            .is_none_or(|text| !text.trim().is_empty())
    });
    match (inline.as_deref(), path) {
        (None, None) => KeyPresence::Absent,
        (Some(_), Some(_)) => KeyPresence::Both,
        (Some(pem), None) => KeyPresence::One(PushKeySource::Inline(pem.to_owned())),
        (None, Some(path)) => KeyPresence::One(PushKeySource::Path(path.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_unconfigured(result: Result<Option<impl Sized>, ConfigError>, label: &str) {
        match result {
            Ok(None) => {}
            Ok(Some(_)) => panic!("{label} was treated as configured"),
            Err(error) => panic!("{label} was rejected: {error}"),
        }
    }

    #[test]
    fn an_empty_provider_is_unconfigured() {
        assert_unconfigured(ApnsPushConfig::default().resolve(), "empty apns");
        assert_unconfigured(FcmPushConfig::default().resolve(), "empty fcm");
        let blanks = ApnsPushConfig {
            team_id: Some(String::new()),
            key_id: Some(String::new()),
            private_key: Some(String::new()),
            private_key_path: Some(PathBuf::new()),
            bundle_id: Some(String::new()),
        };
        assert_unconfigured(blanks.resolve(), "blank apns");
        let whitespace = ApnsPushConfig {
            team_id: Some(" ".into()),
            key_id: Some("\n".into()),
            private_key: Some(" \n".into()),
            private_key_path: Some(" ".into()),
            bundle_id: Some("\t".into()),
        };
        assert_unconfigured(whitespace.resolve(), "whitespace apns");
    }

    #[test]
    fn a_partial_provider_fails_without_echoing_the_key() {
        let config = ApnsPushConfig {
            team_id: Some("TEAM".into()),
            private_key: Some("PRIVATE_SENTINEL".into()),
            ..ApnsPushConfig::default()
        };
        let Err(error) = config.resolve() else {
            panic!("partial apns config was accepted");
        };
        let error = error.to_string();
        assert!(error.contains("push.apns"));
        assert!(!error.contains("PRIVATE_SENTINEL"));
        assert!(!error.contains("TEAM"));
    }

    #[test]
    fn both_key_sources_are_incomplete() {
        let config = FcmPushConfig {
            project_id: Some("proj".into()),
            client_email: Some("svc@example.com".into()),
            package_name: Some("org.matrixaxon.axon".into()),
            private_key: Some("PRIVATE_SENTINEL".into()),
            private_key_path: Some("/run/secrets/fcm.pem".into()),
        };
        let Err(error) = config.resolve() else {
            panic!("both fcm key sources were accepted");
        };
        let error = error.to_string();
        assert!(error.contains("push.fcm"));
        assert!(!error.contains("PRIVATE_SENTINEL"));
    }

    #[test]
    fn a_complete_provider_keeps_the_one_key_source() {
        let config = ApnsPushConfig {
            team_id: Some("TEAM".into()),
            key_id: Some("KEY".into()),
            bundle_id: Some("org.matrixaxon.axon".into()),
            private_key_path: Some("/run/secrets/apns.p8".into()),
            ..ApnsPushConfig::default()
        };
        let resolved = match config.resolve() {
            Ok(Some(resolved)) => resolved,
            Ok(None) => panic!("complete apns config was treated as empty"),
            Err(error) => panic!("complete apns config was rejected: {error}"),
        };
        assert_eq!(resolved.team_id, "TEAM");
        assert!(matches!(resolved.key, PushKeySource::Path(path) if path.ends_with("apns.p8")));
    }
}
