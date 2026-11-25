//! The local, account-wide graph used to group joined rooms by space.

use std::time::Instant;

use axon_store::{SpaceGraphLink, Store, SPACE_GRAPH_MAX_PAGE};
use axum::extract::State;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::cursor;
use crate::extract::{Path, Query};
use crate::response::{ApiError, ApiResponse};

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SpaceGraphQuery {
    /// Page size (default 500, maximum 1,000).
    pub limit: Option<i64>,
    /// Opaque continuation from this account's previous page.
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpaceLinkSource {
    Child,
    ParentClaim,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SpaceGraphLinkDto {
    pub parent_room_id: String,
    pub child_room_id: String,
    /// A link advertised by a space or a verified claim made in the child.
    pub source: SpaceLinkSource,
    /// Valid Matrix sibling order, present only on advertised child links.
    pub order: Option<String>,
    /// Origin timestamp of the state event in milliseconds.
    pub origin_ts: i64,
    /// Whether this verified parent claim names a canonical parent.
    pub canonical: bool,
}

impl From<SpaceGraphLink> for SpaceGraphLinkDto {
    fn from(link: SpaceGraphLink) -> Self {
        let (parent_room_id, child_room_id, source) = if link.key.event_type == "m.space.child" {
            (link.key.room_id, link.key.state_key, SpaceLinkSource::Child)
        } else {
            (
                link.key.state_key,
                link.key.room_id,
                SpaceLinkSource::ParentClaim,
            )
        };
        Self {
            parent_room_id,
            child_room_id,
            source,
            order: link.sort_order,
            origin_ts: link.origin_ts,
            canonical: link.canonical,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SpaceGraphPageDto {
    pub links: Vec<SpaceGraphLinkDto>,
    /// Opaque continuation. Null on the last page.
    pub next_cursor: Option<String>,
}

/// Read joined space relationships
///
/// Returns locally synced links between visible joined rooms for one account.
/// Combine all pages with the room list to find roots and descendants. Pages
/// follow state-tuple identity, so sort siblings after reading them by valid
/// order (absent last), origin timestamp, and child ID. Shared and cyclic edges
/// are returned as stored. Verified parent claims can inform breadcrumbs and
/// canonical-parent navigation. An unknown or inactive account yields an empty
/// page. The server never contacts the homeserver for this read.
///
/// A `room_details.changed` frame with source `space_graph` invalidates the
/// whole graph for its `account_id`. Use the account ID alone to route this
/// source. The frame's `room_id` names one of the affected rooms. Re-read
/// from the first page on invalidation and reconnect.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/spaces/graph",
    params(
        ( "account_id" = Uuid, Path, description = "Axon account id"),
        SpaceGraphQuery,
    ),
    responses(
        (status = 200, description = "A page of joined space relationships", body = ApiResponse<SpaceGraphPageDto>),
        (status = 400, description = "Malformed cursor or a cursor from another account", body = crate::response::ErrorResponse),
        (status = 401, description = "Missing or invalid Axon bearer token", body = crate::response::ErrorResponse),
        (status = 500, description = "Internal database failure", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn space_graph(
    State(store): State<Store>,
    Path(account_id): Path<Uuid>,
    Query(query): Query<SpaceGraphQuery>,
) -> Result<ApiResponse<SpaceGraphPageDto>, ApiError> {
    let after = query
        .cursor
        .as_deref()
        .map(|cursor| {
            cursor::decode_space_graph(account_id, cursor)
                .ok_or_else(|| ApiError::bad_request("invalid cursor"))
        })
        .transpose()?;
    let limit = query.limit.unwrap_or(500).clamp(1, SPACE_GRAPH_MAX_PAGE);
    let started_at = Instant::now();
    let page = store
        .space_graph_page(account_id, after.as_ref(), limit)
        .await?;
    let next_cursor = page
        .links
        .last()
        .filter(|_| page.has_more)
        .map(|link| cursor::encode_space_graph(account_id, &link.key));
    tracing::debug!(%account_id, limit, resumed = after.is_some(), returned = page.links.len(),
        has_more = page.has_more, elapsed_ms = started_at.elapsed().as_millis(), "joined space graph page");
    Ok(ApiResponse::new(SpaceGraphPageDto {
        links: page
            .links
            .into_iter()
            .map(SpaceGraphLinkDto::from)
            .collect(),
        next_cursor,
    }))
}
