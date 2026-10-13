//! Consumer-owned port and wire types for Matrix OAuth browser sign-in
//! (ADR 0117).
//!
//! A sibling of the QR acquisition surface with its own flow DTO and stage
//! enum, so `finalizing` never appears in the QR stage list that existing
//! clients match exhaustively and the QR-only fields are simply absent.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Stable browser sign-in stages exposed by `/v1/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatrixOAuthBrowserStage {
    Starting,
    /// The user must open `verification_uri` and approve the sign-in there.
    WaitingForAuthorization,
    /// The authorization server has issued the session and Axon is adopting
    /// it. The link and user code are gone, and the flow can no longer be
    /// usefully cancelled.
    Finalizing,
    Done,
    Failed,
    Cancelled,
}

/// Create one pre-account browser sign-in flow.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateMatrixOAuthBrowserRequest {
    /// Canonical Matrix user ID the completed OAuth session must belong to.
    /// Its server name is where the homeserver is discovered.
    #[schema(max_length = 1024)]
    pub expected_user_id: String,
}

/// Replayable, presentation-safe state of one browser sign-in. Optional
/// fields are omitted unless they belong to the current stage.
#[derive(Clone, Serialize, ToSchema)]
pub struct MatrixOAuthBrowserFlowDto {
    pub flow_id: Uuid,
    pub expected_user_id: String,
    pub stage: MatrixOAuthBrowserStage,
    /// The link to open in a browser, on any device. Present only in
    /// `waiting_for_authorization`. Anyone holding it can approve the
    /// sign-in, so present it as something to open, not to share, and show
    /// its host so the user can see where it leads.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 2048)]
    pub verification_uri: Option<String>,
    /// The short code the authorization server asks the user to confirm.
    /// Present only in `waiting_for_authorization`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 64)]
    pub user_code: Option<String>,
    /// The signed-in account, present only in `done`. It is active but its
    /// device is **not** verified: lead the user on to recovery-key import or
    /// interactive verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Uuid>,
    /// Why the flow failed, present only in `failed`. An open set: treat a
    /// code you do not recognize as a generic failure. Codes specific to this
    /// method are `authorization_denied` (the user refused) and
    /// `authorization_expired` (nobody approved in time).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// HTTP-shaped errors returned by the server adapter.
#[derive(Debug)]
pub enum MatrixOAuthBrowserError {
    InvalidRequest(String),
    Conflict(String),
    NotFound(String),
    TooMany(String),
    Internal,
}

/// Drives browser sign-in without exposing matrix-rust-sdk to `axon-api`.
#[async_trait]
pub trait MatrixOAuthBrowserService: Send + Sync {
    async fn create(
        &self,
        expected_user_id: &str,
    ) -> Result<MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserError>;

    async fn get(
        &self,
        flow_id: Uuid,
    ) -> Result<MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserError>;

    async fn cancel(&self, flow_id: Uuid) -> Result<(), MatrixOAuthBrowserError>;
}

/// Default used by API unit tests that do not exercise browser sign-in.
pub(crate) struct NoopMatrixOAuthBrowser;

#[async_trait]
impl MatrixOAuthBrowserService for NoopMatrixOAuthBrowser {
    async fn create(
        &self,
        _expected_user_id: &str,
    ) -> Result<MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserError> {
        Err(MatrixOAuthBrowserError::Internal)
    }

    async fn get(
        &self,
        _flow_id: Uuid,
    ) -> Result<MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserError> {
        Err(MatrixOAuthBrowserError::Internal)
    }

    async fn cancel(&self, _flow_id: Uuid) -> Result<(), MatrixOAuthBrowserError> {
        Err(MatrixOAuthBrowserError::Internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(stage: MatrixOAuthBrowserStage) -> MatrixOAuthBrowserFlowDto {
        MatrixOAuthBrowserFlowDto {
            flow_id: Uuid::nil(),
            expected_user_id: "@alice:example.org".to_owned(),
            stage,
            verification_uri: None,
            user_code: None,
            account_id: None,
            error_code: None,
        }
    }

    #[test]
    fn stages_serialize_to_their_stable_names() {
        let cases = [
            (MatrixOAuthBrowserStage::Starting, "starting"),
            (
                MatrixOAuthBrowserStage::WaitingForAuthorization,
                "waiting_for_authorization",
            ),
            (MatrixOAuthBrowserStage::Finalizing, "finalizing"),
            (MatrixOAuthBrowserStage::Done, "done"),
            (MatrixOAuthBrowserStage::Failed, "failed"),
            (MatrixOAuthBrowserStage::Cancelled, "cancelled"),
        ];
        for (stage, expected) in cases {
            assert_eq!(serde_json::to_value(stage).unwrap(), expected);
        }
    }

    #[test]
    fn flow_serialization_omits_data_from_other_stages_and_qr_only_fields() {
        let mut waiting = flow(MatrixOAuthBrowserStage::WaitingForAuthorization);
        waiting.verification_uri = Some("https://auth.example.org/link?code=WDJB".to_owned());
        waiting.user_code = Some("WDJB".to_owned());
        let json = serde_json::to_value(waiting).expect("serialize flow");
        assert_eq!(json["stage"], "waiting_for_authorization");
        assert_eq!(
            json["verification_uri"],
            "https://auth.example.org/link?code=WDJB"
        );
        assert_eq!(json["user_code"], "WDJB");
        for absent in [
            "account_id",
            "error_code",
            "presentation",
            "qr_code_data",
            "check_code",
            "authorization_user_code",
        ] {
            assert!(json.get(absent).is_none(), "{absent} must be omitted");
        }

        let json = serde_json::to_value(flow(MatrixOAuthBrowserStage::Finalizing)).unwrap();
        for absent in ["verification_uri", "user_code", "account_id", "error_code"] {
            assert!(json.get(absent).is_none(), "{absent} must be omitted");
        }
    }

    #[test]
    fn create_request_rejects_unknown_fields() {
        let create = serde_json::from_value::<CreateMatrixOAuthBrowserRequest>(serde_json::json!({
            "expected_user_id": "@alice:example.org",
            "presentation": "display"
        }));
        assert!(create.is_err());
    }
}
