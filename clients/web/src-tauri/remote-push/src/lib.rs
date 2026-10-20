//! APNs (iOS and iPadOS) and FCM (Android) for a suspended shell (ADR 0113).
//!
//! The commands exist on every platform so one capability file serves all of
//! them. Only iOS and Android implement a device token. Desktop, including a
//! Mac app that has been quit, answers [`Capabilities::provider`] `None` and
//! the other commands answer [`ErrorKind::Unsupported`].
//!
//! Nothing here logs a device token, a notification body, or an access token.

use serde::{Deserialize, Serialize};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_remote_push);

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "org.matrixaxon.axon.remotepush";

/// Which provider this binary can register, if any.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// `apns` on iOS and iPadOS, `fcm` on Android, absent everywhere else.
    pub provider: Option<Provider>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Apns,
    Fcm,
}

/// A device token the page can PUT.
///
/// `environment` is `sandbox` or `production` for APNs and absent for FCM.
/// The page omits an absent environment from the request body.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceToken {
    token: String,
    /// `sandbox` or `production` for APNs. Absent for FCM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveredEventIds {
    event_ids: Vec<String>,
}

/// A tap the page can route. Snake case matches the provider payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Wake {
    pub account_id: String,
    pub room_id: String,
    pub event_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostLocalArgs {
    title: String,
    body: String,
    collapse_id: String,
    account_id: String,
    room_id: String,
    event_id: String,
    thread_root_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    Unsupported,
    Failed,
}

impl Error {
    fn unsupported() -> Self {
        Self {
            kind: ErrorKind::Unsupported,
            message: "remote push is not available on this platform".into(),
        }
    }

    #[cfg(any(target_os = "ios", target_os = "android"))]
    fn failed(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Failed,
            message: message.into(),
        }
    }
}

#[cfg(any(target_os = "ios", target_os = "android"))]
impl From<tauri::plugin::mobile::PluginInvokeError> for Error {
    fn from(_error: tauri::plugin::mobile::PluginInvokeError) -> Self {
        // The native side reports a fixed phrase. Do not forward the raw
        // plugin error: a device token must not land in a message.
        Self::failed("the remote push command failed")
    }
}

type Result<T> = std::result::Result<T, Error>;

#[cfg(any(target_os = "ios", target_os = "android"))]
struct Native<R: Runtime>(tauri::plugin::PluginHandle<R>);

#[cfg(any(target_os = "ios", target_os = "android"))]
fn native<R: Runtime>(app: &tauri::AppHandle<R>) -> &tauri::plugin::PluginHandle<R> {
    use tauri::Manager as _;
    &app.state::<Native<R>>().inner().0
}

#[tauri::command]
fn capabilities() -> Capabilities {
    Capabilities {
        provider: provider_for_target(),
    }
}

fn provider_for_target() -> Option<Provider> {
    #[cfg(target_os = "ios")]
    {
        return Some(Provider::Apns);
    }
    #[cfg(target_os = "android")]
    {
        return Some(Provider::Fcm);
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        None
    }
}

#[tauri::command]
async fn device_token<R: Runtime>(app: tauri::AppHandle<R>) -> Result<DeviceToken> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        Ok(native(&app)
            .run_mobile_plugin_async("deviceToken", ())
            .await?)
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        let _ = app;
        Err(Error::unsupported())
    }
}

#[tauri::command]
async fn delivered_event_ids<R: Runtime>(app: tauri::AppHandle<R>) -> Result<DeliveredEventIds> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        Ok(native(&app)
            .run_mobile_plugin_async("deliveredEventIds", ())
            .await?)
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        let _ = app;
        Err(Error::unsupported())
    }
}

#[tauri::command]
async fn pending_wake<R: Runtime>(app: tauri::AppHandle<R>) -> Result<Option<Wake>> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        Ok(native(&app)
            .run_mobile_plugin_async("pendingWake", ())
            .await?)
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        let _ = app;
        Err(Error::unsupported())
    }
}

#[tauri::command]
async fn post_local<R: Runtime>(app: tauri::AppHandle<R>, args: PostLocalArgs) -> Result<()> {
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        native(&app)
            .run_mobile_plugin_async::<serde_json::Value>("postLocal", args)
            .await?;
        Ok(())
    }
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        let _ = (app, args);
        Err(Error::unsupported())
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("remote-push")
        .invoke_handler(tauri::generate_handler![
            capabilities,
            device_token,
            delivered_event_ids,
            pending_wake,
            post_local
        ])
        .setup(|app, api| {
            #[cfg(target_os = "ios")]
            {
                use tauri::Manager as _;
                let handle = api.register_ios_plugin(init_plugin_remote_push)?;
                app.manage(Native(handle));
            }
            #[cfg(target_os = "android")]
            {
                use tauri::Manager as _;
                let handle = api.register_android_plugin(PLUGIN_IDENTIFIER, "RemotePushPlugin")?;
                app.manage(Native(handle));
            }
            #[cfg(not(any(target_os = "ios", target_os = "android")))]
            {
                let _ = (app, api);
            }
            Ok(())
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_follow_the_host() {
        let caps = capabilities();
        #[cfg(target_os = "ios")]
        assert_eq!(caps.provider, Some(Provider::Apns));
        #[cfg(target_os = "android")]
        assert_eq!(caps.provider, Some(Provider::Fcm));
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        assert_eq!(caps.provider, None);
    }
}
