//! Room metadata reads from the fixed, bounded local state projection, and
//! the homeserver-acquired details beside it (ADR 0126).

use std::sync::Arc;

use axon_core::{JobActivity, RoomDetailJob};
use axon_store::{DetailKind, Store};
use axum::extract::State;
use uuid::Uuid;

use crate::extract::Path;
use crate::response::{ApiError, ApiResponse};
use crate::room_details::{self, RoomDetailAcquirer, RoomHomeserverDetailsDto};
use crate::room_metadata::RoomMetadataDto;

/// Get room metadata
///
/// Read typed room metadata from account-scoped cached state.
/// No upstream request is made on this route, and it never waits for one.
/// Missing snapshots remain unknown, including for unjoined/unknown rooms.
/// For a joined room, a snapshot with no tuple also reports whether the
/// account's homeserver has confirmed the room has no such state
/// (`absence_confirmed_at`) and the state of confirming it (`absence_check`);
/// a read schedules the confirmation in the background when it is due.
/// Content is limited to 128 KiB per state tuple before transfer and decoding.
/// Re-read on reopen, on reconnect, and on a `room_details.changed` WebSocket
/// frame whose `sources` include `metadata`, which follows every change to a
/// stored snapshot or confirmation and the end of any confirmation a read saw
/// queued or running; open panels need no polling.
/// Event `origin_ts` is provenance, not a sync-freshness timestamp.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/metadata",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Typed cached state snapshots with availability and provenance", body = ApiResponse<RoomMetadataDto>),
        (status = 401, description = "Missing or invalid bearer token", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal database failure", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn room_metadata(
    State(store): State<Store>,
    State(acquirer): State<Arc<dyn RoomDetailAcquirer>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<RoomMetadataDto>, ApiError> {
    let rows = store
        .room_metadata_states(account_id, &room_id, RoomMetadataDto::EVENT_TYPES)
        .await?;
    let mut metadata = RoomMetadataDto::from_rows(account_id, rows);
    let absence = store
        .room_detail_snapshot(account_id, &room_id, &[DetailKind::Absence])
        .await?;
    let row = |state_type: &str| absence.rows.iter().find(|row| row.state_type == state_type);
    let mut targets = metadata.absence_targets();
    let due = targets.iter().any(|(state_type, snapshot)| {
        snapshot.has_no_tuple() && row(state_type).is_none_or(|row| row.due)
    });
    // Queue first, so this response already shows the confirmation coming.
    if absence.eligible && due {
        acquirer.request(account_id, &room_id, RoomDetailJob::Absence);
    }
    let activity = if absence.eligible {
        acquirer.activity(account_id, &room_id, RoomDetailJob::Absence)
    } else {
        JobActivity::Idle
    };
    for (state_type, snapshot) in &mut targets {
        let (confirmed_at, check) = room_details::absence_fields(row(state_type), activity);
        snapshot.set_absence(confirmed_at, check);
    }
    Ok(ApiResponse::new(metadata))
}

/// Get room homeserver details
///
/// Read the room details only the account's homeserver knows, from Axon's
/// account-scoped cache: the aliases the homeserver maintains for the room,
/// which are distinct from the advertised aliases in `GET …/metadata`, and
/// whether its directory publishes the room.
/// Returns immediately and never waits for the homeserver. When a detail is
/// unknown or expired and no backoff is in force, the read schedules a
/// refresh in the background; `refresh` reports it, and a failed refresh
/// leaves the last answer visible with its `expires_at` in the past.
/// Only rooms the account has joined are acquired; any other room, including
/// unknown room IDs, reads as `unknown` and makes no request.
/// Re-read on reopen, on reconnect, and on a `room_details.changed` WebSocket
/// frame whose `sources` include `homeserver_details`, which follows every
/// change and the end of any refresh a read saw queued or running.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/homeserver_details",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Cached homeserver details with freshness and refresh state", body = ApiResponse<RoomHomeserverDetailsDto>),
        (status = 401, description = "Missing or invalid bearer token", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal database failure", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn room_homeserver_details(
    State(store): State<Store>,
    State(acquirer): State<Arc<dyn RoomDetailAcquirer>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<RoomHomeserverDetailsDto>, ApiError> {
    let kinds = [
        (DetailKind::LocalAliases, RoomDetailJob::LocalAliases),
        (
            DetailKind::DirectoryVisibility,
            RoomDetailJob::DirectoryVisibility,
        ),
    ];
    let snapshot = store
        .room_detail_snapshot(account_id, &room_id, &kinds.map(|(kind, _)| kind))
        .await?;
    let row = |kind: DetailKind| snapshot.rows.iter().find(|row| row.kind == kind.as_str());
    let activity = kinds.map(|(kind, job)| {
        if !snapshot.eligible {
            return JobActivity::Idle;
        }
        if row(kind).is_none_or(|row| row.due) {
            acquirer.request(account_id, &room_id, job);
        }
        acquirer.activity(account_id, &room_id, job)
    });
    Ok(ApiResponse::new(RoomHomeserverDetailsDto {
        local_aliases: room_details::local_aliases(row(DetailKind::LocalAliases), activity[0]),
        directory_visibility: room_details::directory_visibility(
            row(DetailKind::DirectoryVisibility),
            activity[1],
        ),
    }))
}
