//! Sign in with Apple and Keychain storage for the iOS shell (ADR 0054).
//!
//! Two things the webview cannot do for itself on iOS:
//!
//! - **Native Sign in with Apple.** `ASAuthorizationController` is the only
//!   way to get an Apple identity token without a browser round trip and a
//!   registered web callback, and it is what App Review expects of an iOS app
//!   offering Apple sign-in. The server contract is `docs/apple-oauth-native.md`:
//!   the page asks the Axon server for a challenge, hands its `nonce` to
//!   [`apple_sign_in`] *verbatim*, and redeems the identity token that comes
//!   back. Nothing here talks to the Axon server, and nothing here keeps the
//!   identity token: it goes back to the page and nowhere else.
//! - **Keychain storage** for Axon's own access and refresh tokens, so they are
//!   held by the OS credential store rather than in the webview's
//!   `localStorage`. Only Axon-issued credentials pass through here; Apple's
//!   upstream tokens are never stored anywhere (ADR 0054).
//!
//! The commands exist on every platform so that one capability file serves all
//! of them, but only iOS implements them. Everywhere else [`capabilities`]
//! answers `false` for both and the rest answer [`ErrorKind::Unsupported`], and
//! the page never calls them.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_native_auth);

/// What this platform can do, asked once at startup.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    apple_sign_in: bool,
    secure_storage: bool,
}

/// How a command failed, in a shape the page can branch on.
///
/// `message` never carries a token, nonce, or Keychain value: the Swift side
/// reports only Apple's error codes and OSStatus numbers, and nothing here
/// adds to them.
#[derive(Debug, Serialize)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// The user dismissed the Apple sheet. Not a failure to report.
    Cancelled,
    /// This platform has no implementation.
    Unsupported,
    Failed,
}

impl Error {
    #[cfg_attr(target_os = "ios", allow(dead_code))]
    fn unsupported() -> Self {
        Self {
            kind: ErrorKind::Unsupported,
            message: "not available on this platform".into(),
        }
    }
}

#[cfg(target_os = "ios")]
impl From<tauri::plugin::mobile::PluginInvokeError> for Error {
    fn from(error: tauri::plugin::mobile::PluginInvokeError) -> Self {
        use tauri::plugin::mobile::PluginInvokeError;
        match error {
            PluginInvokeError::InvokeRejected(response) => Self {
                kind: if response.code.as_deref() == Some("cancelled") {
                    ErrorKind::Cancelled
                } else {
                    ErrorKind::Failed
                },
                message: response
                    .message
                    .unwrap_or_else(|| "native sign-in failed".into()),
            },
            other => Self {
                kind: ErrorKind::Failed,
                message: other.to_string(),
            },
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppleSignInResponse {
    identity_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SecretEntries {
    entries: HashMap<String, String>,
}

#[cfg(target_os = "ios")]
struct Native<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(target_os = "ios")]
fn native<R: Runtime>(app: &tauri::AppHandle<R>) -> &tauri::plugin::PluginHandle<R> {
    use tauri::Manager as _;
    &app.state::<Native<R>>().inner().0
}

#[tauri::command]
fn capabilities() -> Capabilities {
    let ios = cfg!(target_os = "ios");
    Capabilities {
        apple_sign_in: ios,
        secure_storage: ios,
    }
}

/// Present the Sign in with Apple sheet and return Apple's identity token.
///
/// `nonce` is the Axon server's challenge nonce, which the Swift side passes to
/// `ASAuthorizationAppleIDRequest.nonce` unchanged. It is already a digest of
/// server randomness; hashing it again here would make the signed claim
/// disagree with what the server expects, and every sign-in would fail.
#[tauri::command]
async fn apple_sign_in<R: Runtime>(
    app: tauri::AppHandle<R>,
    nonce: String,
) -> Result<AppleSignInResponse> {
    #[cfg(target_os = "ios")]
    {
        #[derive(Serialize)]
        struct Args {
            nonce: String,
        }
        Ok(native(&app)
            .run_mobile_plugin_async("appleSignIn", Args { nonce })
            .await?)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, nonce);
        Err(Error::unsupported())
    }
}

/// Every entry this app keeps in the Keychain, read once at startup.
#[tauri::command]
async fn secret_load<R: Runtime>(app: tauri::AppHandle<R>) -> Result<SecretEntries> {
    #[cfg(target_os = "ios")]
    {
        Ok(native(&app)
            .run_mobile_plugin_async("secretLoad", ())
            .await?)
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = app;
        Err(Error::unsupported())
    }
}

#[tauri::command]
async fn secret_set<R: Runtime>(
    app: tauri::AppHandle<R>,
    key: String,
    value: String,
) -> Result<()> {
    #[cfg(target_os = "ios")]
    {
        #[derive(Serialize)]
        struct Args {
            key: String,
            value: String,
        }
        native(&app)
            .run_mobile_plugin_async::<serde_json::Value>("secretSet", Args { key, value })
            .await?;
        Ok(())
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, key, value);
        Err(Error::unsupported())
    }
}

#[tauri::command]
async fn secret_delete<R: Runtime>(app: tauri::AppHandle<R>, key: String) -> Result<()> {
    #[cfg(target_os = "ios")]
    {
        #[derive(Serialize)]
        struct Args {
            key: String,
        }
        native(&app)
            .run_mobile_plugin_async::<serde_json::Value>("secretDelete", Args { key })
            .await?;
        Ok(())
    }
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (app, key);
        Err(Error::unsupported())
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("native-auth")
        .invoke_handler(tauri::generate_handler![
            capabilities,
            apple_sign_in,
            secret_load,
            secret_set,
            secret_delete
        ])
        .setup(|_app, _api| {
            #[cfg(target_os = "ios")]
            {
                use tauri::Manager as _;
                let handle = _api.register_ios_plugin(init_plugin_native_auth)?;
                _app.manage(Native(handle));
            }
            Ok(())
        })
        .build()
}
