//! Extractor wrappers that surface rejections through the API error envelope.
//!
//! axum's stock [`Path`](axum::extract::Path) / [`Query`](axum::extract::Query)
//! reject malformed input (e.g. a path segment or query value that isn't a valid
//! UUID) with a plain-text `400` that bypasses our `{ "error": … }` envelope.
//! These newtype wrappers delegate to the stock extractors but map any rejection
//! into [`ApiError::bad_request`], so *every* `/v1/` error — handler-raised or
//! extractor-raised — has the same JSON shape.

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::response::ApiError;

/// Drop-in replacement for [`axum::extract::Path`] whose rejection is an
/// [`ApiError`] (`400`) rather than axum's default plain-text body.
pub struct Path<T>(pub T);

impl<T, S> FromRequestParts<S> for Path<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Path(value)),
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

/// Drop-in replacement for [`axum::extract::Query`] whose rejection is an
/// [`ApiError`] (`400`) rather than axum's default plain-text body.
pub struct Query<T>(pub T);

impl<T, S> FromRequestParts<S> for Query<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Query(value)),
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

/// Drop-in replacement for [`axum::Json`] whose rejection is an [`ApiError`]
/// (`400`) — a malformed or missing request body returns the same
/// `{ "error": … }` envelope as every other failure rather than axum's default
/// plain-text body. Body-consuming, so it must be the last extractor in a handler.
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Json(value)),
            Err(rejection) if rejection.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE => {
                Err(ApiError::payload_too_large("request body too large"))
            }
            Err(rejection) => Err(ApiError::bad_request(rejection.body_text())),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        extract::DefaultBodyLimit,
        http::{Request, StatusCode},
        routing::post,
        Router,
    };
    use serde_json::Value;
    use tower::ServiceExt;

    use super::Json;

    async fn accept_json(Json(_): Json<Value>) {}

    #[tokio::test]
    async fn json_body_limit_uses_the_api_error_envelope() {
        let app = Router::new()
            .route("/", post(accept_json))
            .layer(DefaultBodyLimit::max(8));
        let response = app
            .oneshot(
                Request::post("/")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"long":"body"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "payload_too_large");
    }
}
