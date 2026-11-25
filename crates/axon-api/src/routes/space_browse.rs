//! Explicit nested browse and local sidebar membership reads.

use axon_core::{JobActivity, RoomDetailJob};

use crate::RoomDetailAcquirer;
use axon_store::{HierarchyDepth, SpaceBrowseLink, Store, SPACE_BROWSE_MAX_PAGE};
use axum::extract::State;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::cursor::{decode_space_browse, encode_space_browse};
use crate::extract::{Path, Query};
use crate::response::{ApiError, ApiResponse};
use crate::space_discovery::{DiscoveredRoomDto, SpaceDiscoveryDto};

const BROWSE_JOB: RoomDetailJob = RoomDetailJob::BrowseHierarchy;

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SpaceBrowseQuery {
    /// Page size (default 50, maximum 100).
    pub limit: Option<i64>,
    /// Opaque continuation from the preceding page.
    pub cursor: Option<String>,
}

impl SpaceBrowseQuery {
    fn limit(&self) -> i64 {
        self.limit.unwrap_or(50).clamp(1, SPACE_BROWSE_MAX_PAGE)
    }

    fn after(&self, account_id: Uuid, root: &str) -> Result<Option<(String, String)>, ApiError> {
        self.cursor
            .as_deref()
            .map(|cursor| {
                decode_space_browse(account_id, root, cursor)
                    .ok_or_else(|| ApiError::bad_request("invalid cursor"))
            })
            .transpose()
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SpaceBrowseLinkDto {
    pub parent_room_id: String,
    pub child_room_id: String,
    /// Whether the account has joined the child. Use the synced room list for its display fields.
    pub joined: bool,
    pub order: Option<String>,
    pub origin_ts: i64,
    pub suggested: bool,
    /// From this root's fresh homeserver crawl. Null for joined children.
    pub discovered: Option<DiscoveredRoomDto>,
}

impl From<SpaceBrowseLink> for SpaceBrowseLinkDto {
    fn from(link: SpaceBrowseLink) -> Self {
        Self {
            parent_room_id: link.parent_room_id,
            child_room_id: link.child_room_id,
            joined: link.joined,
            order: link.order,
            origin_ts: link.origin_ts,
            suggested: link.suggested,
            discovered: link.discovered.and_then(DiscoveredRoomDto::from_row),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SpaceBrowsePageDto {
    pub links: Vec<SpaceBrowseLinkDto>,
    pub next_cursor: Option<String>,
    pub discovery: SpaceDiscoveryDto,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DiscoveredSpaceMembershipDto {
    /// The joined space where browsing started.
    pub space_id: String,
    /// A currently joined room reached through at least one unjoined space.
    pub room_id: String,
    /// Unix milliseconds when the sidebar should stop using this membership.
    pub expires_at: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DiscoveredSpaceMembershipPageDto {
    pub memberships: Vec<DiscoveredSpaceMembershipDto>,
    pub next_cursor: Option<String>,
}

fn check_space_id(room_id: &str) -> Result<(), ApiError> {
    if room_id.len() > 255
        || !room_id.starts_with('!')
        || room_id.bytes().any(|b| b.is_ascii_control())
    {
        return Err(ApiError::bad_request("invalid room id"));
    }
    Ok(())
}

/// Read cached nested browse results
///
/// Pages a joined root's fresh browse cache by parent and child room ID.
/// Use the synced room list for joined room details. Restart from the first
/// page on a `room_details.changed` frame with source `space_browse` for
/// this root or `space_graph` for this account. This read is local only.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/space/browse",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Joined root space id"),
        SpaceBrowseQuery,
    ),
    responses(
        (status = 200, description = "Cached nested browse links and crawl progress", body = ApiResponse<SpaceBrowsePageDto>),
        (status = 400, description = "Malformed room id or cursor", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn space_browse(
    State(store): State<Store>,
    State(acquirer): State<Arc<dyn RoomDetailAcquirer>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Query(q): Query<SpaceBrowseQuery>,
) -> Result<ApiResponse<SpaceBrowsePageDto>, ApiError> {
    check_space_id(&room_id)?;
    let after = q.after(account_id, &room_id)?;
    let limit = q.limit();
    let (mut links, crawl) = tokio::try_join!(
        store.space_browse_links(
            account_id,
            &room_id,
            after.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            limit
        ),
        store.hierarchy_crawl_snapshot(account_id, &room_id, HierarchyDepth::Nested),
    )?;
    let next_cursor = (links.len() > limit as usize).then(|| {
        links.truncate(limit as usize);
        let last = links.last().unwrap();
        encode_space_browse(
            account_id,
            &room_id,
            &last.parent_room_id,
            &last.child_room_id,
        )
    });
    let activity = if crawl.eligible {
        acquirer.activity(account_id, &room_id, BROWSE_JOB)
    } else {
        JobActivity::Idle
    };
    Ok(ApiResponse::new(SpaceBrowsePageDto {
        links: links.into_iter().map(Into::into).collect(),
        next_cursor,
        discovery: SpaceDiscoveryDto::new(&crawl, activity),
    }))
}

/// Request nested discovery for a joined space
///
/// Explicit browsing can queue a bounded background crawl. Axon returns
/// immediately. Poll the local GET or re-read on a live invalidation. A fresh
/// crawl, queued work, or backoff suppresses a duplicate request.
#[utoipa::path(
    post,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/space/browse",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Joined root space id"),
    ),
    responses(
        (status = 200, description = "Nested browse crawl status", body = ApiResponse<SpaceDiscoveryDto>),
        (status = 400, description = "Malformed room id", body = crate::response::ErrorResponse),
        (status = 404, description = "No visible joined space at this root", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn request_space_browse(
    State(store): State<Store>,
    State(acquirer): State<Arc<dyn RoomDetailAcquirer>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<SpaceDiscoveryDto>, ApiError> {
    check_space_id(&room_id)?;
    let crawl = store
        .hierarchy_crawl_snapshot(account_id, &room_id, HierarchyDepth::Nested)
        .await?;
    if !crawl.eligible {
        return Err(ApiError::not_found("joined space not found"));
    }
    let mut activity = acquirer.activity(account_id, &room_id, BROWSE_JOB);
    if crawl.due && activity == JobActivity::Idle {
        acquirer.request(account_id, &room_id, BROWSE_JOB);
        activity = acquirer.activity(account_id, &room_id, BROWSE_JOB);
    }
    Ok(ApiResponse::new(SpaceDiscoveryDto::new(&crawl, activity)))
}

/// Read cached space memberships for the sidebar
///
/// Lists joined descendants reached through fresh browsed unjoined paths.
/// Combine this with the joined graph and room list. Use `expires_at` to
/// remove stale memberships and restart paging on reconnect or on account
/// invalidations with source `space_graph` or `space_browse`. This read is
/// local only, including when the cache is empty or expired.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/spaces/discovered",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        SpaceBrowseQuery,
    ),
    responses(
        (status = 200, description = "Cached joined room memberships via unjoined spaces", body = ApiResponse<DiscoveredSpaceMembershipPageDto>),
        (status = 400, description = "Malformed cursor", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn discovered_space_memberships(
    State(store): State<Store>,
    Path(account_id): Path<Uuid>,
    Query(q): Query<SpaceBrowseQuery>,
) -> Result<ApiResponse<DiscoveredSpaceMembershipPageDto>, ApiError> {
    let after = q.after(account_id, "")?;
    let limit = q.limit();
    let mut rows = store
        .discovered_space_memberships(
            account_id,
            after.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            limit,
        )
        .await?;
    let next_cursor = (rows.len() > limit as usize).then(|| {
        rows.truncate(limit as usize);
        let last = rows.last().unwrap();
        encode_space_browse(account_id, "", &last.space_id, &last.room_id)
    });
    let memberships = rows
        .into_iter()
        .map(|row| DiscoveredSpaceMembershipDto {
            space_id: row.space_id,
            room_id: row.room_id,
            expires_at: row.expires_at,
        })
        .collect();
    Ok(ApiResponse::new(DiscoveredSpaceMembershipPageDto {
        memberships,
        next_cursor,
    }))
}
