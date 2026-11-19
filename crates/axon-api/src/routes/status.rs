//! Server status endpoint (M10).
//!
//! `GET /v1/status` reports the history-backfill engine's disk-space health via
//! the [`BackfillStatusProvider`] port, so a client can tell when backfill has
//! paused (and re-poll to see it resume). Authenticated like the rest of `/v1`.

use std::sync::Arc;

use axon_core::PushMaterial;
use axon_store::Store;
use axum::extract::State;

use crate::backfill::BackfillStatusProvider;
use crate::build_info::BuildInfo;
use crate::dto::{
    AccountSyncStatusDto, BackfillStatusDto, HistoryGapsStatusDto, HistoryVerifyStatusDto,
    HomeserverGateDto, ManagementStatusDto, PushStatusDto, StatusDto,
};
use crate::homeserver_status::HomeserverStatusProvider;
use crate::response::{ApiError, ApiResponse};
use crate::state::ManagementConfig;
use crate::sync_status::SyncStatusProvider;

/// Get backfill status
///
/// Report backfill status: the disk-space valve state (live free space, whether
/// paused) plus per-account backfill progress; the holes limited syncs left in
/// stored history and their filling, and the one-time pass that verifies
/// stored history (ADR 0140); per-account sync-service status;
/// each homeserver's load gate for background requests (ADR 0122);
/// the running build's identity; whether the management API is served; and
/// which push providers loaded a key.
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
    State(homeserver_provider): State<Arc<dyn HomeserverStatusProvider>>,
    State(build_info): State<BuildInfo>,
    State(management): State<ManagementConfig>,
    State(push): State<Arc<PushMaterial>>,
) -> Result<ApiResponse<StatusDto>, ApiError> {
    let backfill_snapshot = backfill_provider.snapshot();
    // All read the isolated status pool; concurrently, so a saturated pool
    // costs one acquisition deadline rather than three.
    let (gap_counts, verify_counts, progress) = futures_util::future::join3(
        store.history_gap_counts(),
        store.history_verify_counts(),
        store.backfill_progress(),
    )
    .await;
    let gap_counts = match gap_counts {
        Ok(counts) => Some(counts),
        Err(error) => {
            tracing::warn!(
                reason = error.diagnostic_reason(),
                "status history gap counts unavailable"
            );
            None
        }
    };
    let history_gaps =
        HistoryGapsStatusDto::new(gap_counts, backfill_snapshot.gap_events_recovered);
    let history_verify = HistoryVerifyStatusDto::new(match verify_counts {
        Ok(counts) => Some(counts),
        Err(error) => {
            tracing::warn!(
                reason = error.diagnostic_reason(),
                "status history verification counts unavailable"
            );
            None
        }
    });
    let backfill = match progress {
        Ok(progress) => BackfillStatusDto::new(backfill_snapshot, progress),
        Err(error) => {
            tracing::warn!(
                reason = error.diagnostic_reason(),
                "status progress unavailable"
            );
            let mut snapshot = BackfillStatusDto::new(backfill_snapshot, Vec::new());
            snapshot.progress_available = false;
            snapshot
        }
    };
    Ok(ApiResponse::new(StatusDto {
        backfill,
        history_gaps,
        history_verify,
        sync: sync_provider
            .snapshot()
            .into_iter()
            .map(AccountSyncStatusDto::from)
            .collect(),
        homeservers: homeserver_provider
            .snapshot()
            .into_iter()
            .map(HomeserverGateDto::from)
            .collect(),
        build: build_info.into(),
        management: ManagementStatusDto {
            enabled: management.enabled,
        },
        push: PushStatusDto {
            apns: push.apns_configured(),
            fcm: push.fcm_configured(),
        },
    }))
}
