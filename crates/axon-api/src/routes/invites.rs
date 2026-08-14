//! Pending-invite inbox (`GET /v1/invites`, ADR 0091).
//!
//! Cross-account like [`super::rooms::list_rooms`]. Accept and reject reuse
//! the existing join / leave verbs; this module is read-only.

use axon_store::Store;
use axum::extract::State;
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::dto::InviteDto;
use crate::extract::Query;
use crate::response::{ApiError, ApiResponse};

/// Query parameters for `GET /v1/invites`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct InvitesQuery {
    /// Narrow the list to a single account. Omit for all accounts.
    pub account_id: Option<Uuid>,
}

/// List pending room invites across accounts, newest first.
#[utoipa::path(
    get,
    path = "/v1/invites",
    params(InvitesQuery),
    responses(
        (status = 200, description = "Pending invites, newest first", body = ApiResponse<Vec<InviteDto>>),
    ),
    tag = "invites",
)]
pub async fn list_invites(
    State(store): State<Store>,
    Query(q): Query<InvitesQuery>,
) -> Result<ApiResponse<Vec<InviteDto>>, ApiError> {
    let invites = store.list_invites(q.account_id).await?;
    Ok(ApiResponse::new(
        invites.into_iter().map(InviteDto::from).collect(),
    ))
}
