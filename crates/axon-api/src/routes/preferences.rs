//! Instance-wide preferences (ADRs 0103 and 0104).
//!
//! `GET`/`PUT /v1/preferences/{key}` are **not** nested under an account:
//! Axon is one human per process, so preferences such as mixed-account space
//! order and message gestures cannot live in Matrix account data. Unknown keys
//! are `400`. A successful PUT fans out `preferences.changed`; receivers drop
//! frames whose `device_id` is their own.

use axon_core::{LiveFrame, PreferencesFrame};
use axon_store::Store;
use axum::extract::State;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::dto::{PreferenceDto, PutPreferenceRequest, PutPreferenceResponse};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};
use crate::routes::{json_exceeds_byte_cap, MAX_OPAQUE_JSON_BYTES};

/// Opening a new key is a deliberate, allowlisted addition — not a generic KV
/// dump. Each entry must also have a closed value schema in `validate_value`.
const ALLOWED_KEYS: &[&str] = &["space_order", "message_gestures"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpaceOrderValue {
    spaces: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageGesturesValue {
    schema_version: u8,
    bindings: MessageGestureBindings,
    reaction_emoji: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageGestureBindings {
    double_tap: MessageGestureBinding,
    touch_and_hold: MessageGestureBinding,
    swipe_left: MessageGestureBinding,
}

/// A binding is either one action or JSON `null` (the explicit "Off" value).
/// Keeping this distinct from `Option` makes every binding field required:
/// serde otherwise treats an omitted `Option` field exactly like `null`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MessageGestureBinding {
    Action(MessageGestureAction),
    Off(()),
}

impl MessageGestureBinding {
    fn action(&self) -> Option<MessageGestureAction> {
        match self {
            Self::Action(action) => Some(*action),
            Self::Off(()) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
enum MessageGestureAction {
    Reply,
    Thread,
    React,
    Edit,
    Delete,
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

fn validate_message_gestures(value: &serde_json::Value) -> Result<(), ApiError> {
    let parsed: MessageGesturesValue = serde_json::from_value(value.clone()).map_err(|err| {
        ApiError::bad_request(format!("message_gestures has an invalid v1 shape: {err}"))
    })?;
    if parsed.schema_version != 1 {
        return Err(ApiError::bad_request(format!(
            "message_gestures schema_version must be 1, got {}",
            parsed.schema_version
        )));
    }

    let mut seen = HashSet::new();
    for action in [
        parsed.bindings.double_tap.action(),
        parsed.bindings.touch_and_hold.action(),
        parsed.bindings.swipe_left.action(),
    ]
    .into_iter()
    .flatten()
    {
        if !seen.insert(action) {
            return Err(ApiError::bad_request(
                "message_gestures cannot map one action to multiple gestures",
            ));
        }
    }

    if parsed.reaction_emoji.len() > 64 || emojis::get(&parsed.reaction_emoji).is_none() {
        return Err(ApiError::bad_request(
            "message_gestures reaction_emoji must be one Unicode emoji",
        ));
    }
    Ok(())
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
        "message_gestures" => validate_message_gestures(value),
        other => Err(ApiError::bad_request(format!(
            "preference key {other:?} has no value schema"
        ))),
    }
}

/// Read one instance preference. An allowlisted key that has never been
/// written is a `404` so a client can tell "unset" from an empty value
/// (ADR 0103's one-shot upload of local `spaceOrder`).
#[utoipa::path(
    get,
    path = "/v1/preferences/{key}",
    params(
        ("key" = String, Path, description = "Preference key: `space_order` or `message_gestures`"),
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
        ("key" = String, Path, description = "Preference key: `space_order` or `message_gestures`"),
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
    if json_exceeds_byte_cap(&body.value, MAX_OPAQUE_JSON_BYTES) {
        return Err(ApiError::bad_request(format!(
            "preference value exceeds {MAX_OPAQUE_JSON_BYTES} bytes"
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
    use super::validate_value;
    use crate::routes::json_exceeds_byte_cap;
    use serde_json::json;
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

    #[test]
    fn validate_value_rejects_a_key_with_no_schema() {
        assert!(
            validate_value("theme", &json!({})).is_err(),
            "an allowlisted-but-unschematized key must 400, not accept arbitrary JSON"
        );
    }

    fn message_gestures(
        double_tap: serde_json::Value,
        touch_and_hold: serde_json::Value,
        swipe_left: serde_json::Value,
        reaction_emoji: &str,
    ) -> serde_json::Value {
        json!({
            "schema_version": 1,
            "bindings": {
                "double_tap": double_tap,
                "touch_and_hold": touch_and_hold,
                "swipe_left": swipe_left,
            },
            "reaction_emoji": reaction_emoji,
        })
    }

    #[test]
    fn message_gestures_accepts_the_default_and_all_off() {
        assert!(validate_value(
            "message_gestures",
            &message_gestures(json!("react"), json!("thread"), json!("reply"), "👍")
        )
        .is_ok());
        assert!(validate_value(
            "message_gestures",
            &message_gestures(json!(null), json!(null), json!(null), "👨‍👩‍👧‍👦")
        )
        .is_ok());
        assert!(validate_value(
            "message_gestures",
            &message_gestures(json!("edit"), json!("delete"), json!(null), "🚀")
        )
        .is_ok());
    }

    #[test]
    fn message_gestures_rejects_duplicate_actions() {
        assert!(validate_value(
            "message_gestures",
            &message_gestures(json!("reply"), json!(null), json!("reply"), "👍"),
        )
        .is_err());
    }

    #[test]
    fn message_gestures_rejects_incomplete_or_unknown_shapes() {
        for value in [
            json!({
                "schema_version": 1,
                "bindings": {
                    "double_tap": "react",
                    "touch_and_hold": "thread",
                },
                "reaction_emoji": "👍",
            }),
            json!({
                "schema_version": 1,
                "bindings": {
                    "double_tap": "react",
                    "touch_and_hold": "thread",
                    "swipe_left": "reply",
                    "swipe_right": "delete",
                },
                "reaction_emoji": "👍",
            }),
            message_gestures(json!("share"), json!("thread"), json!("reply"), "👍"),
            json!({
                "schema_version": 1,
                "bindings": {
                    "double_tap": "react",
                    "touch_and_hold": "thread",
                    "swipe_left": "reply",
                },
                "reaction_emoji": "👍",
                "extra": true,
            }),
        ] {
            assert!(validate_value("message_gestures", &value).is_err());
        }
    }

    #[test]
    fn message_gestures_rejects_unknown_version_or_non_emoji() {
        let mut unknown_version =
            message_gestures(json!("react"), json!("thread"), json!("reply"), "👍");
        unknown_version["schema_version"] = json!(2);
        assert!(validate_value("message_gestures", &unknown_version).is_err());

        for reaction in ["", "thumbs up", "👍👍"] {
            assert!(validate_value(
                "message_gestures",
                &message_gestures(json!("react"), json!("thread"), json!("reply"), reaction)
            )
            .is_err());
        }
    }

    #[test]
    fn json_exceeds_byte_cap_stops_at_the_limit() {
        assert!(!json_exceeds_byte_cap(&json!({ "spaces": [] }), 64 * 1024));
        let big = "x".repeat(64 * 1024 + 1);
        assert!(json_exceeds_byte_cap(
            &json!({ "spaces": [big] }),
            64 * 1024
        ));
    }
}
