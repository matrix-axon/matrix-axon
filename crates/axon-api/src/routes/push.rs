//! Device push registration (ADR 0113).
//!
//! `PUT` and `DELETE /v1/push/registrations` sit behind the same bearer gate
//! as the rest of `/v1/`. They do not log the request body or the device
//! token. A registration is owned by the credential session, so a refreshed
//! access token can still delete the row it registered.

use std::sync::Arc;

use axon_store::{NewPushRegistration, PushRegistrationOutcome, Store};
use axum::extract::State;
use axum::Extension;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::auth::AuthedToken;
use crate::dto::{PushRegistrationDto, PutPushRegistrationRequest};
use crate::extract::{Json, Path};
use crate::response::{ApiError, ApiResponse};

/// Reject a device token longer than this before any write. The database
/// check is the same bound, in bytes.
const MAX_TOKEN_BYTES: usize = 4096;

/// Why `token` must not be written. `None` means it is acceptable.
///
/// Postgres `TEXT` rejects a NUL and the request would otherwise become a 500.
/// The other ASCII controls are rejected with it. Leading or trailing
/// whitespace is rejected too: `" abc"` and `"abc"` would otherwise be two
/// devices, and a whitespace-only token is not a device token. The stored
/// value is the bytes the client sent; this does not trim.
fn unacceptable_token(token: &str) -> Option<&'static str> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        return Some("length");
    }
    if token.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Some("control");
    }
    if token != token.trim() {
        return Some("whitespace");
    }
    None
}

/// Body cap for `PUT /v1/push/registrations`. A 4096-byte token of escaped
/// quotes is about twice that, plus the other fields.
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024;

/// Register or re-home a device token.
///
/// Upsert on `(provider, token)`. The same session updates disclosure,
/// environment, and the access token that last wrote the row. A different
/// session becomes the owner and keeps the same id. The response does not
/// include the token.
#[utoipa::path(
    put,
    path = "/v1/push/registrations",
    request_body = PutPushRegistrationRequest,
    responses(
        (status = 200, description = "The registration, without the device token", body = ApiResponse<PushRegistrationDto>),
        (status = 400, description = "The provider, token, disclosure, environment, or session cannot be registered", body = crate::response::ErrorResponse),
    ),
    tag = "push",
)]
pub async fn put_registration(
    State(store): State<Store>,
    State(changed): State<Arc<Notify>>,
    Extension(caller): Extension<AuthedToken>,
    Json(body): Json<PutPushRegistrationRequest>,
) -> Result<ApiResponse<PushRegistrationDto>, ApiError> {
    let provider = match body.provider.as_str() {
        "apns" | "fcm" => body.provider.as_str(),
        _ => {
            tracing::info!(
                code = "push_provider_unsupported",
                "push registration rejected"
            );
            return Err(ApiError::push_provider_unsupported());
        }
    };
    if let Some(reason) = unacceptable_token(&body.token) {
        tracing::info!(
            token_bytes = body.token.len(),
            reason,
            "push registration rejected: token"
        );
        return Err(ApiError::bad_request(
            "token must be 1 to 4096 bytes without surrounding whitespace or control characters",
        ));
    }
    if !matches!(
        body.disclosure.as_str(),
        "activity" | "sender_and_room" | "preview"
    ) {
        tracing::info!("push registration rejected: disclosure");
        return Err(ApiError::bad_request(
            "disclosure must be activity, sender_and_room, or preview",
        ));
    }
    let environment = match (provider, body.environment.as_deref()) {
        ("apns", Some("sandbox" | "production")) => body.environment.as_deref(),
        ("fcm", None) => None,
        _ => {
            tracing::info!(
                code = "push_environment_invalid",
                provider,
                "push registration rejected"
            );
            return Err(ApiError::push_environment_invalid());
        }
    };
    // The store holds the credential lock across the liveness check and the
    // write. An expiring access token with no live refresh token is a legacy
    // or half-minted session: registering it would reap the row when the
    // access token expires, and a suspended app cannot PUT again.
    let outcome = store
        .upsert_push_registration(&NewPushRegistration {
            session_id: caller.session_id,
            token_id: caller.id,
            provider,
            token: &body.token,
            environment,
            disclosure: &body.disclosure,
        })
        .await?;
    let registration = match outcome {
        PushRegistrationOutcome::Stored(registration) => registration,
        PushRegistrationOutcome::RefreshRequired => {
            tracing::info!(
                token_id = %caller.id,
                code = "push_session_refresh_required",
                "push registration rejected"
            );
            return Err(ApiError::push_session_refresh_required());
        }
        PushRegistrationOutcome::CredentialEnded => {
            tracing::info!(
                token_id = %caller.id,
                "push registration rejected: credential ended"
            );
            return Err(ApiError::unauthorized(
                "The bearer token is no longer valid.",
            ));
        }
    };
    tracing::info!(
        registration_id = %registration.id,
        provider = %registration.provider,
        "push registration upserted"
    );
    changed.notify_one();
    Ok(ApiResponse::new(PushRegistrationDto {
        id: registration.id,
        provider: registration.provider,
        disclosure: registration.disclosure,
        environment: registration.environment,
    }))
}

/// Delete the registration when this session owns it.
///
/// An unknown id, or another session's id, is also `204`. The route is
/// idempotent and does not reveal whether some other device has that id.
#[utoipa::path(
    delete,
    path = "/v1/push/registrations/{id}",
    params(
        ("id" = Uuid, Path, description = "Registration id"),
    ),
    responses(
        (status = 204, description = "The registration is gone, or this session did not own it"),
    ),
    tag = "push",
)]
pub async fn delete_registration(
    State(store): State<Store>,
    State(changed): State<Arc<Notify>>,
    Extension(caller): Extension<AuthedToken>,
    Path(id): Path<Uuid>,
) -> Result<axum::http::StatusCode, ApiError> {
    let removed = store
        .delete_push_registration(id, caller.session_id)
        .await?;
    if removed {
        tracing::info!(registration_id = %id, "push registration deleted");
        changed.notify_one();
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::unacceptable_token;

    #[test]
    fn a_token_with_a_control_character_is_rejected_before_a_write() {
        assert_eq!(unacceptable_token("device-token"), None);
        assert_eq!(unacceptable_token(""), Some("length"));
        assert_eq!(unacceptable_token(&"x".repeat(4097)), Some("length"));
        assert_eq!(unacceptable_token("device\0token"), Some("control"));
        assert_eq!(unacceptable_token("device\ntoken"), Some("control"));
        assert_eq!(unacceptable_token("device\u{7f}token"), Some("control"));
        assert_eq!(unacceptable_token("   "), Some("whitespace"));
        assert_eq!(unacceptable_token(" device-token"), Some("whitespace"));
        assert_eq!(unacceptable_token("device-token "), Some("whitespace"));
        assert_eq!(unacceptable_token("device token"), None);
    }
}
