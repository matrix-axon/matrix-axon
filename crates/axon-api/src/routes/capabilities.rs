//! Optional features this server supports (ADR 0132 section 8).
//!
//! `GET /v1/capabilities` lets a client turn on a feature only against a
//! server that has it. An older server has no such route, and its `404` means
//! none of them. Authenticated like the rest of `/v1`.

use crate::dto::{CapabilitiesDto, MediaUploadKindDto};
use crate::response::ApiResponse;

/// Get server capabilities
///
/// The optional features this server supports. A server without this route
/// supports none of them.
#[utoipa::path(
    get,
    path = "/v1/capabilities",
    responses(
        (status = 200, description = "This server's optional features", body = ApiResponse<CapabilitiesDto>),
    ),
    tag = "status",
)]
pub async fn get_capabilities() -> ApiResponse<CapabilitiesDto> {
    ApiResponse::new(CapabilitiesDto {
        edit_url_previews: true,
        media_upload_kinds: MediaUploadKindDto::ALL.to_vec(),
    })
}
