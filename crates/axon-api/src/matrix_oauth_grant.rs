//! Consumer-owned port and wire types for account-scoped Matrix OAuth QR grants.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::matrix_oauth_acquire::{MatrixOAuthQrPresentation, MatrixOAuthQrStage};

/// Create one QR login-grant flow for an existing trusted account.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateMatrixOAuthQrGrantRequest {
    pub presentation: MatrixOAuthQrPresentation,
}

/// Replayable, presentation-safe state of one QR login-grant flow.
///
/// The Matrix access/refresh tokens and exported E2EE secrets never cross this
/// boundary. Optional fields are omitted unless they belong to the current
/// stage.
#[derive(Clone, Serialize, ToSchema)]
pub struct MatrixOAuthQrGrantFlowDto {
    pub flow_id: Uuid,
    pub account_id: Uuid,
    pub presentation: MatrixOAuthQrPresentation,
    pub stage: MatrixOAuthQrStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr_code_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// HTTP-shaped errors returned by the server adapter.
#[derive(Debug)]
pub enum MatrixOAuthQrGrantError {
    InvalidRequest(String),
    Conflict(String),
    NotFound(String),
    TooMany(String),
    Unavailable(String),
    Internal,
}

/// Drives Matrix OAuth QR grants without exposing matrix-rust-sdk to `axon-api`.
#[async_trait]
pub trait MatrixOAuthQrGrantService: Send + Sync {
    async fn create(
        &self,
        account_id: Uuid,
        presentation: MatrixOAuthQrPresentation,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError>;

    async fn get(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError>;

    async fn submit_scan(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
        qr_code_data: &str,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError>;

    async fn submit_check_code(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
        check_code: &str,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError>;

    async fn cancel(&self, account_id: Uuid, flow_id: Uuid) -> Result<(), MatrixOAuthQrGrantError>;
}

/// Default used by API unit tests that do not exercise QR grants.
pub(crate) struct NoopMatrixOAuthQrGrant;

#[async_trait]
impl MatrixOAuthQrGrantService for NoopMatrixOAuthQrGrant {
    async fn create(
        &self,
        _account_id: Uuid,
        _presentation: MatrixOAuthQrPresentation,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        Err(MatrixOAuthQrGrantError::Internal)
    }

    async fn get(
        &self,
        _account_id: Uuid,
        _flow_id: Uuid,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        Err(MatrixOAuthQrGrantError::Internal)
    }

    async fn submit_scan(
        &self,
        _account_id: Uuid,
        _flow_id: Uuid,
        _qr_code_data: &str,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        Err(MatrixOAuthQrGrantError::Internal)
    }

    async fn submit_check_code(
        &self,
        _account_id: Uuid,
        _flow_id: Uuid,
        _check_code: &str,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        Err(MatrixOAuthQrGrantError::Internal)
    }

    async fn cancel(
        &self,
        _account_id: Uuid,
        _flow_id: Uuid,
    ) -> Result<(), MatrixOAuthQrGrantError> {
        Err(MatrixOAuthQrGrantError::Internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_serialization_exposes_only_presentation_safe_data() {
        let flow = MatrixOAuthQrGrantFlowDto {
            flow_id: Uuid::nil(),
            account_id: Uuid::nil(),
            presentation: MatrixOAuthQrPresentation::Scan,
            stage: MatrixOAuthQrStage::WaitingForAuthorization,
            qr_code_data: None,
            check_code: None,
            verification_uri: Some("https://auth.example.org/device".to_owned()),
            error_code: None,
        };
        let json = serde_json::to_value(flow).expect("serialize flow");
        assert_eq!(json["stage"], "waiting_for_authorization");
        assert_eq!(json["verification_uri"], "https://auth.example.org/device");
        assert!(json.get("qr_code_data").is_none());
        assert!(json.get("check_code").is_none());
        assert!(json.get("authorization_user_code").is_none());
    }

    #[test]
    fn create_request_rejects_unknown_fields() {
        let request = serde_json::from_value::<CreateMatrixOAuthQrGrantRequest>(
            serde_json::json!({ "presentation": "display", "approve": true }),
        );
        assert!(request.is_err());
    }
}
