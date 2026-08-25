//! Consumer-owned port and wire types for Matrix OAuth QR account acquisition.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Which device presents the QR image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatrixOAuthQrPresentation {
    Display,
    Scan,
}

/// Stable flow stages exposed by `/v1/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatrixOAuthQrStage {
    Starting,
    QrReady,
    CheckCodeToDisplay,
    CheckCodeRequired,
    WaitingForAuthorization,
    SyncingSecrets,
    Done,
    Failed,
    Cancelled,
}

/// Create one pre-account QR login flow.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateMatrixOAuthQrRequest {
    /// Canonical Matrix user ID the completed OAuth session must belong to.
    #[schema(max_length = 1024)]
    pub expected_user_id: String,
    pub presentation: MatrixOAuthQrPresentation,
}

/// Submit decoded, unpadded-base64 QR data to a scan presentation.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitMatrixOAuthQrRequest {
    #[schema(max_length = 8192)]
    pub qr_code_data: String,
}

/// Submit the two decimal digits shown by the scanning device.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitMatrixOAuthCheckCodeRequest {
    #[schema(min_length = 2, max_length = 2, pattern = "^[0-9]{2}$")]
    pub check_code: String,
}

/// Replayable, presentation-safe state of one QR login flow. Optional fields
/// are omitted unless they belong to the current stage.
#[derive(Clone, Serialize, ToSchema)]
pub struct MatrixOAuthQrFlowDto {
    pub flow_id: Uuid,
    pub expected_user_id: String,
    pub presentation: MatrixOAuthQrPresentation,
    pub stage: MatrixOAuthQrStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qr_code_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// HTTP-shaped errors returned by the server adapter.
#[derive(Debug)]
pub enum MatrixOAuthQrError {
    InvalidRequest(String),
    Conflict(String),
    NotFound(String),
    TooMany(String),
    Internal,
}

/// Drives Matrix OAuth QR login without exposing matrix-rust-sdk to `axon-api`.
#[async_trait]
pub trait MatrixOAuthQrAcquireService: Send + Sync {
    async fn create(
        &self,
        expected_user_id: &str,
        presentation: MatrixOAuthQrPresentation,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError>;

    async fn get(&self, flow_id: Uuid) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError>;

    async fn submit_scan(
        &self,
        flow_id: Uuid,
        qr_code_data: &str,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError>;

    async fn submit_check_code(
        &self,
        flow_id: Uuid,
        check_code: &str,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError>;

    async fn cancel(&self, flow_id: Uuid) -> Result<(), MatrixOAuthQrError>;
}

/// Default used by API unit tests that do not exercise QR login.
pub(crate) struct NoopMatrixOAuthQrAcquire;

#[async_trait]
impl MatrixOAuthQrAcquireService for NoopMatrixOAuthQrAcquire {
    async fn create(
        &self,
        _expected_user_id: &str,
        _presentation: MatrixOAuthQrPresentation,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        Err(MatrixOAuthQrError::Internal)
    }

    async fn get(&self, _flow_id: Uuid) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        Err(MatrixOAuthQrError::Internal)
    }

    async fn submit_scan(
        &self,
        _flow_id: Uuid,
        _qr_code_data: &str,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        Err(MatrixOAuthQrError::Internal)
    }

    async fn submit_check_code(
        &self,
        _flow_id: Uuid,
        _check_code: &str,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        Err(MatrixOAuthQrError::Internal)
    }

    async fn cancel(&self, _flow_id: Uuid) -> Result<(), MatrixOAuthQrError> {
        Err(MatrixOAuthQrError::Internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_serialization_omits_data_from_other_stages() {
        let flow = MatrixOAuthQrFlowDto {
            flow_id: Uuid::nil(),
            expected_user_id: "@alice:example.org".to_owned(),
            presentation: MatrixOAuthQrPresentation::Display,
            stage: MatrixOAuthQrStage::QrReady,
            qr_code_data: Some("opaque".to_owned()),
            check_code: None,
            authorization_user_code: None,
            verification_uri: None,
            account_id: None,
            error_code: None,
        };
        let json = serde_json::to_value(flow).expect("serialize flow");
        assert_eq!(json["stage"], "qr_ready");
        assert_eq!(json["qr_code_data"], "opaque");
        for absent in [
            "check_code",
            "authorization_user_code",
            "verification_uri",
            "account_id",
            "error_code",
        ] {
            assert!(json.get(absent).is_none(), "{absent} must be omitted");
        }
    }

    #[test]
    fn request_dtos_reject_unknown_fields() {
        let create = serde_json::from_value::<CreateMatrixOAuthQrRequest>(serde_json::json!({
            "expected_user_id": "@alice:example.org",
            "presentation": "display",
            "password": "must-not-be-accepted"
        }));
        assert!(create.is_err());

        let scan = serde_json::from_value::<SubmitMatrixOAuthQrRequest>(serde_json::json!({
            "qr_code_data": "opaque",
            "check_code": "12"
        }));
        assert!(scan.is_err());
    }
}
