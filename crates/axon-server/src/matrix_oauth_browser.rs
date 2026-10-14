//! Composition-root adapter for Matrix OAuth browser sign-in (ADR 0117).
//!
//! Browser sign-in is a third method of the acquisition engine that also
//! drives QR login, reached through its own routes and its own DTO.

use async_trait::async_trait;
use axon_api::{
    MatrixOAuthBrowserError, MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserService,
    MatrixOAuthBrowserStage,
};
use axon_sync::{
    MatrixOAuthAcquireEngine, MatrixOAuthAcquireError, MatrixOAuthBrowserStage as SyncBrowserStage,
    MatrixOAuthBrowserState,
};
use uuid::Uuid;

pub struct MatrixOAuthBrowserAdapter(pub MatrixOAuthAcquireEngine);

fn stage_to_api(value: SyncBrowserStage) -> MatrixOAuthBrowserStage {
    match value {
        SyncBrowserStage::Starting => MatrixOAuthBrowserStage::Starting,
        SyncBrowserStage::WaitingForAuthorization => {
            MatrixOAuthBrowserStage::WaitingForAuthorization
        }
        SyncBrowserStage::Finalizing => MatrixOAuthBrowserStage::Finalizing,
        SyncBrowserStage::Done => MatrixOAuthBrowserStage::Done,
        SyncBrowserStage::Failed => MatrixOAuthBrowserStage::Failed,
        SyncBrowserStage::Cancelled => MatrixOAuthBrowserStage::Cancelled,
    }
}

fn state_to_api(value: MatrixOAuthBrowserState) -> MatrixOAuthBrowserFlowDto {
    MatrixOAuthBrowserFlowDto {
        flow_id: value.flow_id,
        expected_user_id: value.expected_user_id,
        stage: stage_to_api(value.stage),
        verification_uri: value.verification_uri,
        user_code: value.user_code,
        account_id: value.account_id,
        error_code: value.error_code,
        cancel_reason: value.cancel_reason.map(str::to_owned),
    }
}

fn map_error(value: MatrixOAuthAcquireError) -> MatrixOAuthBrowserError {
    crate::matrix_oauth_acquire::map_login_error(value, "browser sign-in")
}

#[async_trait]
impl MatrixOAuthBrowserService for MatrixOAuthBrowserAdapter {
    async fn create(
        &self,
        expected_user_id: &str,
    ) -> Result<MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserError> {
        self.0
            .create_browser(expected_user_id)
            .await
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn get(
        &self,
        flow_id: Uuid,
    ) -> Result<MatrixOAuthBrowserFlowDto, MatrixOAuthBrowserError> {
        self.0
            .get_browser(flow_id)
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn cancel(&self, flow_id: Uuid) -> Result<(), MatrixOAuthBrowserError> {
        self.0.cancel_browser(flow_id).map_err(map_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sync_stage_has_a_stable_api_stage() {
        let cases = [
            (
                SyncBrowserStage::Starting,
                MatrixOAuthBrowserStage::Starting,
            ),
            (
                SyncBrowserStage::WaitingForAuthorization,
                MatrixOAuthBrowserStage::WaitingForAuthorization,
            ),
            (
                SyncBrowserStage::Finalizing,
                MatrixOAuthBrowserStage::Finalizing,
            ),
            (SyncBrowserStage::Done, MatrixOAuthBrowserStage::Done),
            (SyncBrowserStage::Failed, MatrixOAuthBrowserStage::Failed),
            (
                SyncBrowserStage::Cancelled,
                MatrixOAuthBrowserStage::Cancelled,
            ),
        ];
        for (sync, api) in cases {
            assert_eq!(stage_to_api(sync), api);
        }
    }

    #[test]
    fn state_maps_field_for_field() {
        let flow_id = Uuid::new_v4();
        let dto = state_to_api(MatrixOAuthBrowserState {
            flow_id,
            expected_user_id: "@alice:example.org".to_owned(),
            stage: SyncBrowserStage::WaitingForAuthorization,
            verification_uri: Some("https://auth.example.org/link".to_owned()),
            user_code: Some("WDJB-MJHT".to_owned()),
            account_id: None,
            error_code: None,
            cancel_reason: None,
        });
        assert_eq!(dto.flow_id, flow_id);
        assert_eq!(dto.expected_user_id, "@alice:example.org");
        assert_eq!(dto.stage, MatrixOAuthBrowserStage::WaitingForAuthorization);
        assert_eq!(
            dto.verification_uri.as_deref(),
            Some("https://auth.example.org/link")
        );
        assert_eq!(dto.user_code.as_deref(), Some("WDJB-MJHT"));
        assert_eq!(dto.cancel_reason, None);

        let replaced = state_to_api(MatrixOAuthBrowserState {
            flow_id,
            expected_user_id: "@alice:example.org".to_owned(),
            stage: SyncBrowserStage::Cancelled,
            verification_uri: None,
            user_code: None,
            account_id: None,
            error_code: None,
            cancel_reason: Some("replaced"),
        });
        assert_eq!(replaced.cancel_reason.as_deref(), Some("replaced"));
    }

    #[test]
    fn login_conflicts_and_wrong_family_ids_keep_their_status_class() {
        for error in [
            MatrixOAuthAcquireError::AccountAlreadyActive,
            MatrixOAuthAcquireError::AccountBeingDeleted,
            MatrixOAuthAcquireError::AccountDraining,
            MatrixOAuthAcquireError::FlowAlreadyExists,
        ] {
            assert!(matches!(
                map_error(error),
                MatrixOAuthBrowserError::Conflict(_)
            ));
        }
        assert!(matches!(
            map_error(MatrixOAuthAcquireError::NotFound(Uuid::nil())),
            MatrixOAuthBrowserError::NotFound(_)
        ));
        assert!(matches!(
            map_error(MatrixOAuthAcquireError::Capacity),
            MatrixOAuthBrowserError::TooMany(_)
        ));
        assert!(matches!(
            map_error(MatrixOAuthAcquireError::InvalidUserId),
            MatrixOAuthBrowserError::InvalidRequest(_)
        ));
    }
}
