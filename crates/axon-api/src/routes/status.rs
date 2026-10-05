//! Server status endpoint (M10).
//!
//! `GET /v1/status` reports the history-backfill engine's disk-space health via
//! the [`BackfillStatusProvider`] port, so a client can tell when backfill has
//! paused (and re-poll to see it resume). Authenticated like the rest of `/v1`.

use std::sync::Arc;

use axon_store::Store;
use axum::extract::State;

use crate::backfill::BackfillStatusProvider;
use crate::build_info::BuildInfo;
use crate::dto::{AccountSyncStatusDto, BackfillStatusDto, ManagementStatusDto, StatusDto};
use crate::response::{ApiError, ApiResponse};
use crate::state::ManagementConfig;
use crate::sync_status::SyncStatusProvider;

/// Report backfill status: the disk-space valve state (live free space, whether
/// paused) plus per-account backfill progress; per-account sync-service status;
/// the running build's identity; and whether the management API is served.
#[utoipa::path(
    get,
    path = "/v1/status",
    responses(
        (status = 200, description = "Current server status", body = ApiResponse<StatusDto>),
    ),
    tag = "status",
)]
pub async fn get_status(
    State(store): State<Store>,
    State(backfill_provider): State<Arc<dyn BackfillStatusProvider>>,
    State(sync_provider): State<Arc<dyn SyncStatusProvider>>,
    State(build_info): State<BuildInfo>,
    State(management): State<ManagementConfig>,
) -> Result<ApiResponse<StatusDto>, ApiError> {
    let progress = store.backfill_progress().await?;
    Ok(ApiResponse::new(StatusDto {
        backfill: BackfillStatusDto::new(backfill_provider.snapshot(), progress),
        sync: sync_provider
            .snapshot()
            .into_iter()
            .map(AccountSyncStatusDto::from)
            .collect(),
        build: build_info.into(),
        management: ManagementStatusDto {
            enabled: management.enabled,
        },
    }))
}
