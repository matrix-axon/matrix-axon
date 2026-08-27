//! axum HTTP and WebSocket handlers; OpenAPI spec via utoipa.
//!
//! `axon-api` owns the axum [`Router`] and all HTTP/WebSocket handlers. It
//! consumes a [`Store`](axon_store::Store) handle via [`AppState`] router state
//! rather than opening its own database connections.
//!
//! Versioned application routes live under `/v1/`. Account-scoped resources nest
//! under `/v1/accounts/{account_id}/…`; `/v1/rooms` is the cross-account
//! aggregate list. Live events stream over the `/v1/ws` WebSocket. `/healthz` is
//! an unversioned operational liveness probe. The response envelope (`{data}` /
//! `{error}`) lives in [`response`]; the OpenAPI document is [`ApiDoc`].

mod auth;
mod backfill;
mod backup_state;
mod build_info;
mod cursor;
mod devices;
mod dto;
mod extract;
mod lifecycle;
mod matrix_oauth_acquire;
mod media;
mod member_profiles;
mod oauth;
mod openapi;
mod response;
mod routes;
mod search;
mod sender;
mod state;
mod sync_state;
mod sync_status;
mod trust;
mod uploads;
mod verification;
mod ws;

pub use auth::{StoreTokenVerifier, TokenVerifier};
pub use axon_core::{Formatted, MediaAttachment, MediaSendKind, Relation};
pub use backfill::{BackfillStatusProvider, BackfillStatusSnapshot};
pub use backup_state::BackupStateProvider;
pub use build_info::BuildInfo;
pub use devices::{DeviceInfo, DeviceList, DeviceListError, DeviceListService};
pub use dto::{BackupSnapshotDto, BackupStateDto, MediaUploadKindDto, RecoveryStateDto};
pub use lifecycle::{
    AccountLifecycle, BackupAction, DeleteError, LoginError, LogoutError, RecoverError,
    RecoverResult, RedecryptUtdsError, RedecryptUtdsStats,
};
pub use matrix_oauth_acquire::{
    MatrixOAuthQrAcquireService, MatrixOAuthQrError, MatrixOAuthQrFlowDto,
    MatrixOAuthQrPresentation, MatrixOAuthQrStage,
};
pub use media::{MediaError, MediaProxy, MediaResource};
pub use member_profiles::{
    MemberProfile, MemberProfileError, MemberProfileService, NoopMemberProfileService,
};
pub use oauth::{
    http_client as oauth_http_client, rate_limit::spawn_sweeper as spawn_oauth_rate_limit_sweeper,
    GenericOidcProvider, OAuthRuntime, OidcError, OidcProvider, UpstreamTokens, VerifiedIdentity,
};
pub use openapi::ApiDoc;
pub use response::{ApiError, ApiResponse, ErrorBody, ErrorResponse};
pub use search::{SearchHit, SearchHits, SearchQuery, SearchQueryError, SearchQueryParams};
pub use sender::{
    AccountActionsSender, EphemeralSender, LeaveOutcome, MembershipSender, MessageSender,
    PowerLevelsSender, RoomEntrySender, RoomSettingsSender, SendError,
};
pub use state::{AppState, BootstrapConfig};
pub use sync_state::SyncStateProvider;
pub use sync_status::{AccountSyncSnapshot, SyncStatusProvider};
pub use trust::{CurrentTrust, SenderTrustService, TrustBundle, TrustError, TrustSnapshot};
pub use uploads::{
    ClaimedUpload, StageUploadError, StageUploadRequest, StagedUpload, StagedUploadService,
    UploadStream,
};
pub use verification::{FlowStage, FlowSummary, VerificationService, VerifyError};

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    middleware::from_fn_with_state,
    response::Html,
    routing::{any, get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use tower_http::compression::{
    predicate::{DefaultPredicate, NotForContentType, Predicate},
    CompressionLayer,
};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::Level;

/// Build the top-level application router over the shared [`AppState`].
///
/// Handlers pull just the state they need (e.g. `State<Store>`) via `FromRef`,
/// so new shared dependencies can be added to `AppState` without touching
/// existing routes.
pub fn router(state: AppState) -> Router {
    // Every `/v1/…` HTTP route requires a valid bearer token (M7b). The guard is
    // a single layer over this sub-router rather than a per-route attachment, so
    // there is no route that can be added without it — including the lifecycle
    // verbs that were loopback-restricted before auth existed. `/healthz` and the
    // WebSocket are assembled outside it (below).
    let verifier = state.verifier.clone();
    // The scan body carries bounded base64 QR data. Cap the JSON body before
    // allocation/parsing rather than relying only on the post-parse field check.
    let matrix_oauth_qr = Router::new()
        .route(
            "/v1/accounts/login/qr",
            post(routes::matrix_oauth_acquire::create),
        )
        .route(
            "/v1/accounts/login/qr/{flow_id}",
            get(routes::matrix_oauth_acquire::get).delete(routes::matrix_oauth_acquire::cancel),
        )
        .route(
            "/v1/accounts/login/qr/{flow_id}/scan",
            post(routes::matrix_oauth_acquire::submit_scan),
        )
        .route(
            "/v1/accounts/login/qr/{flow_id}/check-code",
            post(routes::matrix_oauth_acquire::submit_check_code),
        )
        .layer(DefaultBodyLimit::max(10 * 1024));
    let authed = Router::new()
        .merge(matrix_oauth_qr)
        // Account read API: the cross-account list and a single account.
        .route("/v1/accounts", get(routes::accounts::list_accounts))
        // Runtime login / logout / recover — the secret-bearing lifecycle verbs.
        .route("/v1/accounts/login", post(routes::accounts::login))
        .route("/v1/accounts/import", post(routes::accounts::import_token))
        .route(
            "/v1/accounts/{account_id}/logout",
            post(routes::accounts::logout),
        )
        .route(
            "/v1/accounts/{account_id}/recover",
            post(routes::accounts::recover),
        )
        .route(
            "/v1/accounts/{account_id}/backup/enable",
            post(routes::accounts::enable_backup),
        )
        .route(
            "/v1/accounts/{account_id}/utds/redecrypt",
            post(routes::accounts::redecrypt_utds),
        )
        // Read one account and delete one account (same path, two methods).
        .route(
            "/v1/accounts/{account_id}",
            get(routes::accounts::get_account).delete(routes::accounts::delete_account),
        )
        // Interactive SAS verification: start/list on one path, per-flow read +
        // confirm/cancel below.
        .route(
            "/v1/accounts/{account_id}/verify",
            get(routes::verify::list_flows).post(routes::verify::start_verification),
        )
        .route(
            "/v1/accounts/{account_id}/verify/{flow_id}",
            get(routes::verify::get_flow),
        )
        .route(
            "/v1/accounts/{account_id}/verify/{flow_id}/confirm",
            post(routes::verify::confirm),
        )
        .route(
            "/v1/accounts/{account_id}/verify/{flow_id}/cancel",
            post(routes::verify::cancel),
        )
        // Device-list / discovery (M16, ADR 0060): the picker a client reads
        // before starting SAS verification above. Defaults to the account's
        // own devices; `?user_id=` lists another user's (cross-user, ADR 0040).
        .route(
            "/v1/accounts/{account_id}/devices",
            get(routes::devices::list_devices),
        )
        // Full-text search across the index (M9b). Cross-account by default;
        // narrowed by the query filters. Returns hydrated events + BM25 score.
        .route("/v1/search", get(routes::search::search))
        // Server status (M10): the backfill engine's disk-space health, so a
        // client can tell when backfill has paused.
        .route("/v1/status", get(routes::status::get_status))
        .route("/v1/rooms", get(routes::rooms::list_rooms))
        // Pending invites (ADR 0091). Cross-account like `/v1/rooms`; accept
        // and reject reuse the existing join / leave verbs.
        .route("/v1/invites", get(routes::invites::list_invites))
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/members",
            get(routes::rooms::room_members),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/timeline",
            get(routes::rooms::room_timeline),
        )
        // Relation aggregation reads (M8b): the room's threads and a thread-scoped
        // timeline, reusing the room timeline's cursor pagination.
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/threads",
            get(routes::rooms::room_threads),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/threads/{root_id}/timeline",
            get(routes::rooms::thread_timeline),
        )
        // Room-state read gaps (issue #404, ADR 0084): spaces hierarchy,
        // pinned messages, room info, and the upgrade chain.
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/space/children",
            get(routes::rooms::space_children),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/space/parents",
            get(routes::rooms::space_parents),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/pinned",
            get(routes::rooms::room_pinned),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/info",
            get(routes::rooms::room_info),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/upgrade",
            get(routes::rooms::room_upgrade),
        )
        .route(
            "/v1/accounts/{account_id}/events/{event_id}",
            get(routes::events::get_event),
        )
        // Per-event relation aggregations (M8b): reaction tallies, direct replies,
        // and the forensic edit trail — resolved regardless of pagination.
        .route(
            "/v1/accounts/{account_id}/events/{event_id}/reactions",
            get(routes::events::get_reactions),
        )
        .route(
            "/v1/accounts/{account_id}/events/{event_id}/replies",
            get(routes::events::get_replies),
        )
        .route(
            "/v1/accounts/{account_id}/events/{event_id}/edits",
            get(routes::events::get_edits),
        )
        // Per-event verification bundle (M7c): at-decrypt snapshot + live evidence.
        .route(
            "/v1/accounts/{account_id}/events/{event_id}/verification",
            get(routes::events::get_verification_bundle),
        )
        // Mutations (M6). account_id is nested in the path; the response is the
        // created event id. Edit (PUT) and redact (DELETE) share one path.
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/send",
            post(routes::messages::send_message),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/send-media",
            post(routes::messages::send_media),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}",
            put(routes::messages::edit_message).delete(routes::messages::redact_event),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/events/{event_id}/reactions",
            post(routes::messages::react),
        )
        // Ephemeral outbound signals: real Matrix read receipts (ADR 0067) and
        // typing notices (ADR 0068, M19a). Best-effort from the caller's
        // perspective; server-side these behave like any other mutation.
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/read",
            post(routes::ephemeral::send_read_receipt),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/typing",
            put(routes::ephemeral::send_typing_notice),
        )
        // Existing-room membership (ADR 0068, M19b): leave/forget this
        // account's own membership, and invite/kick/ban/unban other users.
        // All resolve through `SdkGateway::room()` exactly like the M6
        // mutations above; none return an event id (see `MembershipSender`).
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/leave",
            post(routes::membership::leave_room),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/forget",
            post(routes::membership::forget_room),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/invite",
            post(routes::membership::invite_user),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/kick",
            post(routes::membership::kick_user),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/ban",
            post(routes::membership::ban_user),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/unban",
            post(routes::membership::unban_user),
        )
        // Room entry (ADR 0068, M19c): join, knock, create, create-DM. Unlike
        // the M19b membership block above, none of these resolve via
        // `SdkGateway::room()` — there is no `Room` handle until one of these
        // calls produces it — so they go through `RoomEntrySender` straight
        // to `ClientManager::get_or_connect`. Every response carries the
        // resulting room's id (`RoomEntryResultDto`).
        .route(
            "/v1/accounts/{account_id}/rooms/join",
            post(routes::room_entry::join_room),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/knock",
            post(routes::room_entry::knock_room),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/dm",
            post(routes::room_entry::create_dm),
        )
        .route(
            "/v1/accounts/{account_id}/rooms",
            post(routes::room_entry::create_room),
        )
        // Room settings (ADR 0068, M19d): name/topic/avatar/tags. name/topic/
        // avatar resolve through `SdkGateway::room()` exactly like the M19b
        // membership block; tags write room account data, not a state event
        // (see `RoomSettingsSender`'s doc comment). PUT/DELETE rather than
        // POST since these are idempotent field-set/clear operations, unlike
        // M19a/b/c's fire-once actions.
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/name",
            put(routes::room_settings::set_room_name),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/topic",
            put(routes::room_settings::set_room_topic),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/avatar",
            put(routes::room_settings::set_room_avatar)
                .delete(routes::room_settings::remove_room_avatar),
        )
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/tags/{tag}",
            put(routes::room_settings::set_room_tag).delete(routes::room_settings::remove_room_tag),
        )
        // Power levels (ADR 0068, M19e): role thresholds and per-user levels,
        // merged into one `m.room.power_levels` write, split from the M19d
        // block above because a bad write here can permanently strand the
        // caller — see `PowerLevelsSender`'s doc comment. GET returns the
        // fully resolved levels; no generic Tier-2 state read exists yet
        // (ADR 0055) to cover this otherwise.
        .route(
            "/v1/accounts/{account_id}/rooms/{room_id}/power_levels",
            put(routes::power_levels::set_power_levels).get(routes::power_levels::get_power_levels),
        )
        // Account actions (ADR 0068, M19f): this account's own display name
        // and avatar, reading any user's profile, this account's ignore
        // list, and public-room directory search. None of these resolve
        // through a `Room` handle — same `ClientManager::get_or_connect`
        // resolution path as room-entry above. `get_user_profile` and
        // `search_public_rooms` are reads (their envelope carries real data,
        // not an empty object) mixed into the same port as the mutations,
        // the same shape `PowerLevelsSender` already has.
        .route(
            "/v1/accounts/{account_id}/profile/display_name",
            put(routes::account_actions::set_display_name),
        )
        .route(
            "/v1/accounts/{account_id}/profile/avatar",
            put(routes::account_actions::set_account_avatar)
                .delete(routes::account_actions::remove_account_avatar),
        )
        .route(
            "/v1/accounts/{account_id}/users/{user_id}/profile",
            get(routes::account_actions::get_user_profile),
        )
        .route(
            "/v1/accounts/{account_id}/users/{user_id}/ignore",
            put(routes::account_actions::ignore_user)
                .delete(routes::account_actions::unignore_user),
        )
        .route(
            "/v1/accounts/{account_id}/directory/public_rooms",
            get(routes::account_actions::search_public_rooms),
        )
        // Staged media uploads (M15a): client bytes are accepted and stored
        // before the later room-aware send-media mutation consumes them.
        .route(
            "/v1/accounts/{account_id}/media/uploads",
            post(routes::uploads::stage_upload),
        )
        .route(
            "/v1/accounts/{account_id}/media/uploads/{upload_id}",
            axum::routing::delete(routes::uploads::delete_upload),
        )
        // Per-device client state (M12): drafts / read markers, GET the merged
        // cross-device view and PUT a merge-upsert that fans out over /v1/ws.
        .route(
            "/v1/devices/{device_id}/state/{namespace}",
            get(routes::device_state::get_device_state).put(routes::device_state::put_device_state),
        )
        // Media proxy. Authenticated download of an `mxc://` resource through
        // the account's live homeserver connection. Returns raw bytes, not the
        // JSON envelope, so it is not expressible cleanly in the same response
        // schema as the rest of the read API.
        .route(
            "/v1/media/{account_id}/{server_name}/{media_id}",
            get(routes::media::get_media),
        )
        // Homeserver-generated thumbnail variant of the same object (M17,
        // ADR 0063): proxies the Matrix C-S thumbnail endpoint via
        // `matrix_sdk::media::MediaFormat::Thumbnail`, instead of the full
        // original. Plain media only — encrypted media 400s before this
        // handler ever calls the proxy.
        .route(
            "/v1/media/{account_id}/{server_name}/{media_id}/thumbnail",
            get(routes::media::get_media_thumbnail),
        )
        // Keep unmatched `/v1/...` paths inside the authenticated API boundary:
        // they must not fall through to the browser-facing HTML fallback below.
        .route("/v1", get(v1_not_found))
        .route("/v1/{*path}", get(v1_not_found))
        .route_layer(from_fn_with_state(verifier, auth::require_bearer));

    // The third un-gated sibling (M14, ADR 0054): how a client obtains a
    // bearer token in the first place, so it cannot itself sit behind
    // `require_bearer`. Its own `oauth.enabled`/per-provider checks (and this
    // layer's rate limiter) are the boundary instead — see `routes::oauth`.
    let oauth_state = state.oauth.clone();
    let oauth_router = Router::new()
        .route("/v1/oauth/authorize", get(routes::oauth::authorize))
        .route(
            "/v1/oauth/{provider}/callback",
            get(routes::oauth::callback),
        )
        .route("/v1/oauth/token", post(routes::oauth::token))
        .route("/v1/oauth/bind", get(routes::oauth::bind))
        .route_layer(from_fn_with_state(
            oauth_state,
            oauth::rate_limit::rate_limit,
        ));

    let bootstrap_state = state.bootstrap.clone();
    let bootstrap_router = Router::new()
        .route("/bootstrap", any(routes::bootstrap::wrong_url))
        .route("/bootstrap/token", any(routes::bootstrap::wrong_url))
        .route(
            "/bootstrap/oauth/{provider}",
            any(routes::bootstrap::wrong_url),
        )
        .route("/bootstrap/{code}", get(routes::bootstrap::page))
        .route(
            "/bootstrap/{code}/token",
            post(routes::bootstrap::issue_bearer),
        )
        .route(
            "/bootstrap/{code}/oauth/{provider}",
            get(routes::bootstrap::start_oauth),
        )
        .route_layer(from_fn_with_state(
            bootstrap_state,
            routes::bootstrap::require_allowed_peer,
        ));

    Router::new()
        // Unversioned operational liveness probe — no auth (a monitor must reach
        // it without a token).
        .route("/healthz", get(healthz))
        .merge(bootstrap_router)
        .merge(authed)
        // Live event fan-out. Not in the OpenAPI document — a WebSocket upgrade
        // isn't expressible in OpenAPI 3.1; the frame protocol is documented in
        // the `ws` module and ADR 0020. A browser can't set an `Authorization`
        // header on a socket, so the handler authenticates the token itself at
        // upgrade time rather than riding the `require_bearer` layer.
        .route("/v1/ws", get(ws::ws_handler))
        .merge(oauth_router)
        // Human-facing browser fallback for the server root / other non-API
        // unmatched paths. This is intentionally outside the `/v1` subtree so
        // unknown API routes keep auth + JSON semantics above.
        .fallback(browser_fallback)
        // Compress JSON/HTML when the client sends `Accept-Encoding` (issue
        // #86). Inner of the two layers so the access log sees the encoded
        // response. Clients that do not advertise an encoding (the TUI's
        // reqwest, curl without `--compressed`) get the identity body. A
        // reverse proxy that already set Content-Encoding, or that fetches
        // identity from this process and encodes itself, will not double
        // compress — we skip a body that is already encoded.
        .layer(response_compression_layer())
        // Baseline per-request access log (method/path/status/latency) at INFO
        // so it's visible under the default log level, not just RUST_LOG=debug.
        // Without this, a request that stalls inside a handler (e.g. blocked on
        // a semaphore permit or slow disk I/O) leaves no server-side trace that
        // it was ever received at all.
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &axum::http::Request<Body>| {
                    tracing::info_span!(
                        "request",
                        method = %request.method(),
                        path = %request.uri().path(),
                        version = ?request.version(),
                    )
                })
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}

/// gzip / brotli / zstd, negotiated from `Accept-Encoding`.
///
/// `DefaultPredicate` already skips gRPC, SSE, bodies under 32 bytes, and
/// `image/*` (SVG excepted). We additionally skip `audio/*`, `video/*`, and
/// `application/octet-stream` — the media proxy's inline-safe types and the
/// fallback it uses for everything else. Those bodies are already compressed
/// (or opaque binary) and the routes advertise `Accept-Ranges`; compressing
/// them would strip that header and fight `Range` requests. tower-http also
/// refuses to compress a response that already carries `Content-Range`.
fn response_compression_layer() -> CompressionLayer<impl Predicate> {
    CompressionLayer::new().compress_when(response_compression_predicate())
}

fn response_compression_predicate() -> impl Predicate {
    DefaultPredicate::new()
        .and(NotForContentType::const_new("audio/"))
        .and(NotForContentType::const_new("video/"))
        .and(NotForContentType::const_new("application/octet-stream"))
}

/// Liveness probe. Always returns `200 OK` with `{"status":"ok"}` — it does not
/// touch the database, so a transient DB outage does not cause restarts.
async fn healthz() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// JSON `404` for unknown `/v1/...` paths. These stay in the authenticated API
/// boundary instead of falling through to the browser-facing fallback page.
async fn v1_not_found() -> ApiError {
    ApiError::not_found("route not found")
}

/// Browser-facing informational page for unmatched non-API paths.
async fn browser_fallback() -> Html<&'static str> {
    Html(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Axon</title>
  </head>
  <body>
    <h1>Axon</h1>
    <p>Axon is a self-hosted Matrix agent and API server.</p>
    <p>No web interface is served at this address. Use a compatible client such as <code>axon-tui</code>.</p>
    <p>Operational checks live at <code>/healthz</code>. The API is served under <code>/v1/</code>.</p>
  </body>
</html>
"#,
    )
}

#[cfg(test)]
mod compression_predicate_tests {
    use axum::body::Body;
    use axum::http::{header, Response};
    use tower_http::compression::predicate::Predicate;

    use super::response_compression_predicate;

    fn should_compress(content_type: &str, body_len: usize) -> bool {
        let response = Response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from("n".repeat(body_len)))
            .expect("response");
        response_compression_predicate().should_compress(&response)
    }

    /// Large enough to clear `DefaultPredicate`'s 32-byte floor.
    const LARGE: usize = 64;

    #[test]
    fn compresses_json_and_html() {
        assert!(should_compress("application/json", LARGE));
        assert!(should_compress("application/json; charset=utf-8", LARGE));
        assert!(should_compress("text/html; charset=utf-8", LARGE));
    }

    #[test]
    fn skips_bodies_under_the_size_floor() {
        assert!(!should_compress("application/json", 16));
    }

    #[test]
    fn skips_already_compressed_or_ranged_media() {
        assert!(!should_compress("image/png", LARGE));
        assert!(!should_compress("image/jpeg", LARGE));
        assert!(!should_compress("audio/mpeg", LARGE));
        assert!(!should_compress("video/mp4", LARGE));
        assert!(!should_compress("application/octet-stream", LARGE));
    }
}
