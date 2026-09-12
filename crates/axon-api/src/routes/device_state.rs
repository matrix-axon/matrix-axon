//! Per-device client state endpoints (M12, ADR 0048).
//!
//! `GET`/`PUT /v1/devices/{device_id}/state/{namespace}` give clients a generic
//! KV store scoped to `(account, device, namespace, key)` for drafts, read
//! markers, and similar — opaque to axon. A *device* is a client of axon (one
//! TUI/web install) identified by a client-supplied UUID; its first PUT is its
//! registration (no `devices` table). The account is named by a required
//! `account_id` query parameter, keeping the spec's URL while the table stays
//! account-scoped like every other.
//!
//! PUT is a merge-upsert (`null` = delete, stored as a tombstone) and wins by
//! server-clock arrival order; GET returns the last-write-wins merged view
//! across all the account's devices. A successful PUT fans out one
//! `device_state.changed` frame over `/v1/ws`; receivers drop frames carrying
//! their own device id (echo suppression).

use axon_core::{DeviceStateFrame, LiveFrame};
use axon_store::{DeviceStateUpsert, Store};
use axum::extract::State;
use serde::Deserialize;
use tokio::sync::broadcast;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::dto::{
    DeviceStateDto, DeviceStateEntryDto, PutDeviceStateRequest, PutDeviceStateResponse,
};
use crate::extract::{Json, Path, Query};
use crate::response::{ApiError, ApiResponse};
use crate::routes::MAX_OPAQUE_JSON_BYTES;

/// Caps on a PUT (ADR 0048). The values are deliberately opaque — axon can't
/// validate their *shape* without coupling the server to client semantics —
/// so their *size* is bounded instead: without these, a client could park a
/// multi-megabyte blob per key, per device, per namespace, bounded only by
/// the HTTP body limit ("hostile input" boundary rule, AGENTS.md).
/// Per-entry values share [`crate::routes::MAX_OPAQUE_JSON_BYTES`] with
/// instance preferences.
///
/// Max entries in one PUT's merge-upsert. Drafts and read markers write one
/// key at a time; this leaves headroom for batch writers.
const MAX_PUT_ENTRIES: usize = 64;
/// Max bytes per entry key. Keys are typically room ids, which Matrix keeps
/// well under this.
const MAX_KEY_BYTES: usize = 512;
/// Max bytes for the namespace path segment on a write.
const MAX_NAMESPACE_BYTES: usize = 64;

/// Query parameters for both device-state endpoints: the account the state is
/// scoped to. `account_id` is non-optional so the emitted OpenAPI marks it
/// `required`; omitting or mangling it is rejected by the [`Query`] extractor
/// with the standard enveloped `400`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DeviceStateQueryDto {
    /// The Axon account this state belongs to.
    pub account_id: Uuid,
}

/// Confirm the account exists — an unknown account is a `404`, matching the
/// accounts routes.
async fn require_account(store: &Store, account_id: Uuid) -> Result<(), ApiError> {
    store
        .get_account(account_id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("account {account_id} not found")))?;
    Ok(())
}

/// Read one namespace of per-device state: the last-write-wins **merged view
/// across all the account's devices** (per key, the newest write wins; deleted
/// keys are absent). A namespace never written is an empty map, not a `404`.
/// The `device_id` names the calling device; the merge is account-wide.
#[utoipa::path(
    get,
    path = "/v1/devices/{device_id}/state/{namespace}",
    params(
        ("device_id" = Uuid, Path, description = "The calling device (client-supplied UUID)"),
        ("namespace" = String, Path, description = "The state namespace, e.g. `drafts`"),
        DeviceStateQueryDto,
    ),
    responses(
        (status = 200, description = "The merged namespace state", body = ApiResponse<DeviceStateDto>),
        (status = 400, description = "Missing account_id or malformed device_id", body = crate::response::ErrorResponse),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
    ),
    tag = "device-state",
)]
pub async fn get_device_state(
    State(store): State<Store>,
    Path((_device_id, namespace)): Path<(Uuid, String)>,
    Query(DeviceStateQueryDto { account_id }): Query<DeviceStateQueryDto>,
) -> Result<ApiResponse<DeviceStateDto>, ApiError> {
    require_account(&store, account_id).await?;
    let entries = store
        .merged_device_state(account_id, &namespace)
        .await?
        .into_iter()
        .map(|row| (row.key.clone(), DeviceStateEntryDto::from(row)))
        .collect();
    Ok(ApiResponse::new(DeviceStateDto { namespace, entries }))
}

/// Write per-device state: a **merge-upsert** of the supplied entries under the
/// calling device. Only the keys present are touched; a `null` value deletes
/// the key (a tombstone, so the deletion wins the cross-device merge).
/// Last-write-wins on the server clock — the returned `updated_at` is the
/// ordering all devices share. A successful write with at least one entry fans
/// out a `device_state.changed` frame over `/v1/ws` carrying this `device_id`,
/// which sibling devices apply and the originator ignores.
#[utoipa::path(
    put,
    path = "/v1/devices/{device_id}/state/{namespace}",
    params(
        ("device_id" = Uuid, Path, description = "The writing device (client-supplied UUID; first PUT registers it)"),
        ("namespace" = String, Path, description = "The state namespace, e.g. `drafts`"),
        DeviceStateQueryDto,
    ),
    request_body = PutDeviceStateRequest,
    responses(
        (status = 200, description = "The write's server timestamp", body = ApiResponse<PutDeviceStateResponse>),
        (status = 400, description = "Missing account_id, malformed device_id, empty entries, or an over-limit write (more than 64 entries, a value over 64 KiB, a key over 512 bytes, or a namespace over 64 bytes)", body = crate::response::ErrorResponse),
        (status = 404, description = "No such account", body = crate::response::ErrorResponse),
    ),
    tag = "device-state",
)]
pub async fn put_device_state(
    State(store): State<Store>,
    State(live): State<broadcast::Sender<LiveFrame>>,
    Path((device_id, namespace)): Path<(Uuid, String)>,
    Query(DeviceStateQueryDto { account_id }): Query<DeviceStateQueryDto>,
    Json(body): Json<PutDeviceStateRequest>,
) -> Result<ApiResponse<PutDeviceStateResponse>, ApiError> {
    require_account(&store, account_id).await?;
    if body.entries.is_empty() {
        return Err(ApiError::bad_request("entries must not be empty"));
    }
    // Bound the write before touching the store (see the caps above): the
    // values stay opaque, so size — not shape — is what's validated.
    if namespace.len() > MAX_NAMESPACE_BYTES {
        return Err(ApiError::bad_request(format!(
            "namespace exceeds {MAX_NAMESPACE_BYTES} bytes"
        )));
    }
    if body.entries.len() > MAX_PUT_ENTRIES {
        return Err(ApiError::bad_request(format!(
            "too many entries in one write (max {MAX_PUT_ENTRIES})"
        )));
    }
    for (key, value) in &body.entries {
        if key.len() > MAX_KEY_BYTES {
            return Err(ApiError::bad_request(format!(
                "entry key exceeds {MAX_KEY_BYTES} bytes"
            )));
        }
        let value_bytes = value
            .as_ref()
            .map(|v| v.to_string().len())
            .unwrap_or_default();
        if value_bytes > MAX_OPAQUE_JSON_BYTES {
            return Err(ApiError::bad_request(format!(
                "entry value for {key:?} exceeds {MAX_OPAQUE_JSON_BYTES} bytes"
            )));
        }
    }

    // One atomic statement for the whole batch: all entries commit or none
    // do (a failure can never leave a partially-applied merge with no frame),
    // and every row shares one statement-stable `updated_at` — so the single
    // timestamp on the frame and response below is exact, not approximate.
    let entries: Vec<(String, Option<serde_json::Value>)> = body.entries.into_iter().collect();
    let updated_at = store
        .upsert_device_state(&DeviceStateUpsert {
            account_id,
            device_id,
            namespace: &namespace,
            entries: &entries,
        })
        .await?;

    // Fan out one frame for the whole batch. A send error just means no
    // subscriber is connected — normal, not a failure.
    let _ = live.send(LiveFrame::DeviceState(DeviceStateFrame {
        account_id,
        device_id,
        namespace,
        entries,
        updated_at,
    }));

    Ok(ApiResponse::new(PutDeviceStateResponse {
        updated_at: updated_at.to_rfc3339(),
    }))
}
