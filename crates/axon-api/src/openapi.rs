//! The OpenAPI 3.1 document, assembled by utoipa from the handler signatures.
//!
//! [`ApiDoc::openapi`](utoipa::OpenApi::openapi) builds the spec; the
//! `openapi_spec_is_current` test serializes it and diffs it against the
//! checked-in `openapi/openapi.json`, so drift between handlers and the spec is
//! a failing test. The spec is the source of truth for generated clients.

use utoipa::openapi::header::HeaderBuilder;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{ContentBuilder, Object, Ref, RefOr, Response, ResponseBuilder, Type};
use utoipa::{Modify, OpenApi};

/// Injects the bearer-token security scheme (M7b) and documents the `401` every
/// gated operation can now produce. The scheme is referenced by the global
/// `security` requirement below; the `401` is added to each operation so the
/// source-of-truth contract describes both the requirement and its failure shape.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_token",
                SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
            );
        }

        // Add a shared 401 (the enveloped ErrorResponse) to every *gated*
        // operation that doesn't already declare one.
        //
        // Not every `/v1/` route is behind the gate. `/v1/oauth/*` cannot be —
        // it is how a client obtains a bearer token in the first place — and an
        // operation that opts out with an empty `security()` can never answer
        // 401, so claiming it would describe a failure the route has no way to
        // produce. An empty security requirement is the opt-out; absent means
        // the global requirement applies.
        for path_item in openapi.paths.paths.values_mut() {
            let operations = [
                path_item.get.as_mut(),
                path_item.put.as_mut(),
                path_item.post.as_mut(),
                path_item.delete.as_mut(),
                path_item.options.as_mut(),
                path_item.head.as_mut(),
                path_item.patch.as_mut(),
                path_item.trace.as_mut(),
            ];
            for operation in operations.into_iter().flatten() {
                // `security()` with no arguments emits an empty list, which
                // OpenAPI defines as "this operation needs no authentication"
                // — distinct from the field being absent, which inherits the
                // document-level requirement.
                if operation
                    .security
                    .as_ref()
                    .is_some_and(|requirements| requirements.is_empty())
                {
                    continue;
                }
                operation
                    .responses
                    .responses
                    .entry("401".to_owned())
                    .or_insert_with(unauthorized_response);
            }
        }
    }
}

/// The reusable `401 Unauthorized` response: the standard `{ error: { code,
/// message } }` envelope, referencing the `ErrorResponse` component schema, plus
/// the `WWW-Authenticate` challenge the bearer gate emits (RFC 6750 §3) — so the
/// contract describes the failure's headers as well as its body.
fn unauthorized_response() -> RefOr<Response> {
    RefOr::T(
        ResponseBuilder::new()
            .description("Missing, malformed, or revoked bearer token")
            .header(
                "WWW-Authenticate",
                HeaderBuilder::new()
                    .description(Some(
                        "RFC 6750 bearer challenge: `Bearer` for a missing or \
                         malformed credential, `Bearer error=\"invalid_token\"` \
                         for an unknown or revoked token.",
                    ))
                    .schema(Object::with_type(Type::String))
                    .build(),
            )
            .content(
                "application/json",
                ContentBuilder::new()
                    .schema(Some(Ref::from_schema_name("ErrorResponse")))
                    .build(),
            )
            .build(),
    )
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Axon API",
        version = "0.1.0",
        description = "Read API for a personal Matrix state layer. Account-scoped \
                       resources nest under /v1/accounts/{account_id}; /v1/rooms is \
                       the cross-account aggregate.",
    ),
    // Every documented route requires a bearer token (M7b); the scheme itself is
    // registered by `SecurityAddon`.
    modifiers(&SecurityAddon),
    security(("bearer_token" = [])),
    paths(
        crate::routes::accounts::list_accounts,
        crate::routes::accounts::login,
        crate::routes::accounts::import_token,
        crate::routes::accounts::logout,
        crate::routes::accounts::recover,
        crate::routes::accounts::enable_backup,
        crate::routes::accounts::redecrypt_utds,
        crate::routes::accounts::get_account,
        crate::routes::accounts::delete_account,
        crate::routes::matrix_oauth_acquire::create,
        crate::routes::matrix_oauth_acquire::get,
        crate::routes::matrix_oauth_acquire::submit_scan,
        crate::routes::matrix_oauth_acquire::submit_check_code,
        crate::routes::matrix_oauth_acquire::cancel,
        crate::routes::matrix_oauth_grant::create,
        crate::routes::matrix_oauth_grant::get,
        crate::routes::matrix_oauth_grant::submit_scan,
        crate::routes::matrix_oauth_grant::submit_check_code,
        crate::routes::matrix_oauth_grant::cancel,
        crate::routes::oauth::providers,
        crate::routes::search::search,
        crate::routes::status::get_status,
        crate::routes::rooms::list_rooms,
        crate::routes::invites::list_invites,
        crate::routes::rooms::room_members,
        crate::routes::rooms::room_timeline,
        crate::routes::rooms::room_threads,
        crate::routes::rooms::thread_timeline,
        crate::routes::rooms::space_children,
        crate::routes::rooms::space_parents,
        crate::routes::rooms::room_pinned,
        crate::routes::rooms::room_info,
        crate::routes::rooms::room_upgrade,
        crate::routes::events::get_event,
        crate::routes::events::get_reactions,
        crate::routes::events::get_replies,
        crate::routes::events::get_edits,
        crate::routes::events::get_verification_bundle,
        crate::routes::messages::send_message,
        crate::routes::messages::send_media,
        crate::routes::messages::edit_message,
        crate::routes::messages::redact_event,
        crate::routes::messages::react,
        crate::routes::ephemeral::send_read_receipt,
        crate::routes::ephemeral::send_typing_notice,
        crate::routes::membership::leave_room,
        crate::routes::membership::forget_room,
        crate::routes::membership::invite_user,
        crate::routes::membership::kick_user,
        crate::routes::membership::ban_user,
        crate::routes::membership::unban_user,
        crate::routes::room_entry::join_room,
        crate::routes::room_entry::knock_room,
        crate::routes::room_entry::create_dm,
        crate::routes::room_entry::create_room,
        crate::routes::room_settings::set_room_name,
        crate::routes::room_settings::set_room_topic,
        crate::routes::room_settings::set_room_avatar,
        crate::routes::room_settings::remove_room_avatar,
        crate::routes::room_settings::set_room_tag,
        crate::routes::room_settings::remove_room_tag,
        crate::routes::power_levels::set_power_levels,
        crate::routes::power_levels::get_power_levels,
        crate::routes::account_actions::set_display_name,
        crate::routes::account_actions::set_account_avatar,
        crate::routes::account_actions::remove_account_avatar,
        crate::routes::account_actions::get_user_profile,
        crate::routes::account_actions::ignore_user,
        crate::routes::account_actions::unignore_user,
        crate::routes::account_actions::search_public_rooms,
        crate::routes::verify::start_verification,
        crate::routes::verify::list_flows,
        crate::routes::verify::get_flow,
        crate::routes::verify::confirm,
        crate::routes::verify::cancel,
        crate::routes::devices::list_devices,
        crate::routes::media::get_media,
        crate::routes::media::get_media_thumbnail,
        crate::routes::uploads::stage_upload,
        crate::routes::uploads::delete_upload,
        crate::routes::device_state::get_device_state,
        crate::routes::device_state::put_device_state,
    ),
    components(schemas(
        crate::dto::AccountDto,
        crate::dto::AccountStateDto,
        crate::dto::BackupSnapshotDto,
        crate::dto::BackupStateDto,
        crate::dto::RecoveryStateDto,
        crate::dto::BackupActionDto,
        crate::dto::RecoverResponseDto,
        crate::dto::EnableBackupRequest,
        crate::dto::EnableBackupResponseDto,
        crate::dto::RoomDto,
        crate::dto::InviteDto,
        crate::dto::MemberDto,
        crate::dto::EventDto,
        crate::dto::TimelinePage,
        crate::dto::SearchResultDto,
        crate::dto::SearchPage,
        crate::dto::StatusDto,
        crate::dto::BackfillStatusDto,
        crate::dto::AccountBackfillDto,
        crate::dto::BuildInfoDto,
        crate::dto::AccountSyncStatusDto,
        crate::dto::ReactionDto,
        crate::dto::ThreadSummaryDto,
        crate::dto::SpaceChildDto,
        crate::dto::SpaceParentDto,
        crate::dto::RoomInfoDto,
        crate::dto::RoomUpgradeDto,
        crate::dto::LoginRequest,
        crate::dto::ImportTokenRequest,
        crate::dto::RecoverRequest,
        crate::dto::RedecryptUtdsResponse,
        crate::dto::SendMessageRequest,
        crate::dto::SendMediaRequest,
        crate::dto::EditRequest,
        crate::dto::ReactRequest,
        crate::dto::SendResultDto,
        crate::dto::ReadReceiptRequest,
        crate::dto::TypingRequest,
        crate::dto::InviteRequest,
        crate::dto::MemberActionRequest,
        crate::dto::JoinRoomRequest,
        crate::dto::KnockRoomRequest,
        crate::dto::CreateDmRequest,
        crate::dto::RoomPresetDto,
        crate::dto::CreateRoomRequestDto,
        crate::dto::RoomEntryResultDto,
        crate::dto::SetRoomNameRequest,
        crate::dto::SetRoomTopicRequest,
        crate::dto::SetRoomAvatarRequest,
        crate::dto::SetRoomTagRequest,
        crate::dto::PowerLevelChangesRequest,
        crate::dto::PowerLevelsDto,
        crate::dto::SetDisplayNameRequest,
        crate::dto::SetAccountAvatarRequest,
        crate::dto::MatrixProfileDto,
        crate::dto::PublicRoomSummaryDto,
        crate::dto::PublicRoomsPageDto,
        crate::dto::MediaUploadKindDto,
        crate::dto::ThumbnailMethodDto,
        crate::dto::StagedUploadDto,
        crate::dto::StartVerifyRequest,
        crate::dto::StartVerifyResponse,
        crate::dto::FlowDto,
        crate::dto::FlowStageDto,
        crate::dto::EmojiDto,
        crate::dto::VerificationBundleDto,
        crate::dto::TrustSnapshotDto,
        crate::dto::CurrentTrustDto,
        crate::dto::DeviceDto,
        crate::dto::DeviceListDto,
        crate::dto::DeviceStateDto,
        crate::dto::DeviceStateEntryDto,
        crate::dto::PutDeviceStateRequest,
        crate::dto::PutDeviceStateResponse,
        crate::matrix_oauth_acquire::CreateMatrixOAuthQrRequest,
        crate::matrix_oauth_acquire::SubmitMatrixOAuthQrRequest,
        crate::matrix_oauth_acquire::SubmitMatrixOAuthCheckCodeRequest,
        crate::matrix_oauth_acquire::MatrixOAuthQrPresentation,
        crate::matrix_oauth_acquire::MatrixOAuthQrStage,
        crate::matrix_oauth_acquire::MatrixOAuthQrFlowDto,
        crate::matrix_oauth_grant::CreateMatrixOAuthQrGrantRequest,
        crate::matrix_oauth_grant::MatrixOAuthQrGrantFlowDto,
        crate::response::ErrorBody,
        crate::response::ErrorResponse,
    )),
    tags(
        (name = "accounts", description = "The Matrix accounts this Axon manages"),
        (name = "matrix-oauth", description = "Matrix OAuth QR login flows"),
        (name = "rooms", description = "Rooms and their timelines"),
        (name = "invites", description = "Pending room invitations for this Axon's accounts"),
        (name = "search", description = "Full-text search across the index"),
        (name = "status", description = "Server health and backfill status"),
        (name = "events", description = "Individual events"),
        (name = "messages", description = "Sending, editing, redacting, and reacting"),
        (name = "ephemeral", description = "Outbound read receipts and typing notices sent to the homeserver"),
        (name = "membership", description = "Existing-room membership: leave, forget, invite, kick, ban, unban"),
        (name = "room-entry", description = "Joining, knocking on, and creating rooms"),
        (name = "room-settings", description = "Room name, topic, avatar, and this account's tags on a room"),
        (name = "power-levels", description = "Role thresholds and per-user power levels for a room"),
        (name = "account-actions", description = "This account's own profile and ignore list, another user's profile, and public-room directory search"),
        (name = "verification", description = "Interactive SAS device verification"),
        (name = "devices", description = "Device-list / discovery, for the SAS verification picker"),
        (name = "media", description = "Authenticated MXC media proxy"),
        (name = "device-state", description = "Per-device client state: drafts, read markers"),
    ),
)]
pub struct ApiDoc;
