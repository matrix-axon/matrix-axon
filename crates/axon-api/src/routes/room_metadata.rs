//! Room metadata reads from the fixed, bounded local state projection.

use axon_store::Store;
use axum::extract::State;
use uuid::Uuid;

use crate::extract::Path;
use crate::response::{ApiError, ApiResponse};
use crate::room_metadata::RoomMetadataDto;

/// Read typed room metadata from account-scoped cached state.
/// No upstream requests or membership aggregation occur on this route.
/// Missing snapshots remain unknown, including for unjoined/unknown rooms.
/// Content is limited to 64 KiB per state tuple before transfer and decoding.
/// Re-read after relevant state events and on reconnect; event `origin_ts`
/// is provenance, not a last-successful-sync timestamp.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/metadata",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Typed cached state snapshots with availability and provenance", body = ApiResponse<RoomMetadataDto>),
    ),
    tag = "rooms",
)]
pub async fn room_metadata(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<RoomMetadataDto>, ApiError> {
    let rows = store.room_metadata_states(account_id, &room_id).await?;
    Ok(ApiResponse::new(RoomMetadataDto::from_rows(
        account_id, rows,
    )))
}
