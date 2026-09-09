//! Instance-wide preferences (ADR 0103).
//!
//! `GET`/`PUT /v1/preferences/{key}` are **not** nested under an account:
//! Axon is one human per process, and the first key (`space_order`) is a
//! mixed-account sequence that cannot live in Matrix account data. Unknown
//! keys are `400`. A successful PUT fans out `preferences.changed`; receivers
//! drop frames whose `device_id` is their own.

use axon_core::{LiveFrame, PreferencesFrame};
use axon_store::Store;
use axum::extract::State;
use serde::Deserialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::dto::{PreferenceDto, PutPreferenceRequest, PutPreferenceResponse};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};

/// The only key this endpoint accepts today. Opening a new key is a
/// deliberate, allowlisted addition — not a generic KV dump.
const ALLOWED_KEYS: &[&str] = &["space_order"];
/// Size cap matching device_state values (ADR 0048 / ADR 0103).
const MAX_VALUE_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpaceOrderValue {
    spaces: Vec<String>,
}

fn require_allowed_key(key: &str) -> Result<(), ApiError> {
    if ALLOWED_KEYS.contains(&key) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "unknown preference key {key:?}; allowed: {}",
            ALLOWED_KEYS.join(", ")
        )))
    }
}

fn validate_space_order_entry(entry: &str) -> bool {
    let Some((account, room)) = entry.split_once('/') else {
        return false;
    };
    Uuid::parse_str(account).is_ok() && room.starts_with('!') && room.len() > 1
}

fn validate_value(key: &str, value: &serde_json::Value) -> Result<(), ApiError> {
    match key {
        "space_order" => {
            let parsed: SpaceOrderValue = serde_json::from_value(value.clone()).map_err(|err| {
                ApiError::bad_request(format!(
                    "space_order must be {{ \"spaces\": [\"{{account_id}}/{{room_id}}\", ...] }}: {err}"
                ))
            })?;
            for (index, entry) in parsed.spaces.iter().enumerate() {
                if !validate_space_order_entry(entry) {
                    return Err(ApiError::bad_request(format!(
                        "spaces[{index}] is not {{account_id}}/{{room_id}}"
                    )));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Read one instance preference. An allowlisted key that has never been
/// written is a `404` so a client can tell "unset" from an empty value
/// (ADR 0103's one-shot upload of local `spaceOrder`).
#[utoipa::path(
    get,
    path = "/v1/preferences/{key}",
    params(
        ("key" = String, Path, description = "Preference key; currently only `space_order`"),
    ),
    responses(
        (status = 200, description = "The stored preference", body = ApiResponse<PreferenceDto>),
        (status = 400, description = "Unknown preference key", body = crate::response::ErrorResponse),
        (status = 404, description = "Preference has never been written", body = crate::response::ErrorResponse),
    ),
    tag = "preferences",
)]
pub async fn get_preference(
    State(store): State<Store>,
    Path(key): Path<String>,
) -> Result<ApiResponse<PreferenceDto>, ApiError> {
    require_allowed_key(&key)?;
    let row = store
        .instance_preference(&key)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("preference {key} is not set")))?;
    Ok(ApiResponse::new(PreferenceDto {
        key: row.key,
        value: row.value,
        updated_at: row.updated_at.to_rfc3339(),
    }))
}

/// Write one instance preference. Last-write-wins on the whole value. A
/// successful write fans out `preferences.changed` carrying this `device_id`.
#[utoipa::path(
    put,
    path = "/v1/preferences/{key}",
    params(
        ("key" = String, Path, description = "Preference key; currently only `space_order`"),
    ),
    request_body = PutPreferenceRequest,
    responses(
        (status = 200, description = "The write's server timestamp", body = ApiResponse<PutPreferenceResponse>),
        (status = 400, description = "Unknown key, invalid value, or value over 64 KiB", body = crate::response::ErrorResponse),
    ),
    tag = "preferences",
)]
pub async fn put_preference(
    State(store): State<Store>,
    State(live): State<broadcast::Sender<LiveFrame>>,
    Path(key): Path<String>,
    Json(body): Json<PutPreferenceRequest>,
) -> Result<ApiResponse<PutPreferenceResponse>, ApiError> {
    require_allowed_key(&key)?;
    let value_bytes = body.value.to_string().len();
    if value_bytes > MAX_VALUE_BYTES {
        return Err(ApiError::bad_request(format!(
            "preference value exceeds {MAX_VALUE_BYTES} bytes"
        )));
    }
    validate_value(&key, &body.value)?;

    let updated_at = store.upsert_instance_preference(&key, &body.value).await?;

    let _ = live.send(LiveFrame::PreferencesChanged(PreferencesFrame {
        key: key.clone(),
        value: body.value,
        device_id: body.device_id,
    }));

    tracing::info!(key = %key, device_id = %body.device_id, "preferences: put succeeded");

    Ok(ApiResponse::new(PutPreferenceResponse {
        updated_at: updated_at.to_rfc3339(),
    }))
}

#[cfg(test)]
mod tests {
    use super::validate_space_order_entry;
    use uuid::Uuid;

    #[test]
    fn space_order_entry_requires_account_uuid_and_room_id() {
        let account = Uuid::new_v4();
        assert!(validate_space_order_entry(&format!(
            "{account}/!space:localhost"
        )));
        assert!(!validate_space_order_entry("not-a-uuid/!space:localhost"));
        assert!(!validate_space_order_entry(&format!(
            "{account}/space:localhost"
        )));
        assert!(!validate_space_order_entry(&format!("{account}/!")));
        assert!(!validate_space_order_entry("!space:localhost"));
    }
}
