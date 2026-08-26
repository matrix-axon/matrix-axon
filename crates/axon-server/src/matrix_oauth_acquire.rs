//! Composition-root adapter for Matrix OAuth QR account acquisition.

use async_trait::async_trait;
use axon_api::{
    MatrixOAuthQrAcquireService, MatrixOAuthQrError, MatrixOAuthQrFlowDto,
    MatrixOAuthQrPresentation, MatrixOAuthQrStage,
};
use axon_sync::{
    MatrixOAuthAcquireEngine, MatrixOAuthAcquireError, MatrixOAuthAcquirePresentation,
    MatrixOAuthAcquireStage, MatrixOAuthAcquireState,
};
use uuid::Uuid;

pub struct MatrixOAuthAcquireAdapter(pub MatrixOAuthAcquireEngine);

fn presentation_to_sync(value: MatrixOAuthQrPresentation) -> MatrixOAuthAcquirePresentation {
    match value {
        MatrixOAuthQrPresentation::Display => MatrixOAuthAcquirePresentation::Display,
        MatrixOAuthQrPresentation::Scan => MatrixOAuthAcquirePresentation::Scan,
    }
}

fn presentation_to_api(value: MatrixOAuthAcquirePresentation) -> MatrixOAuthQrPresentation {
    match value {
        MatrixOAuthAcquirePresentation::Display => MatrixOAuthQrPresentation::Display,
        MatrixOAuthAcquirePresentation::Scan => MatrixOAuthQrPresentation::Scan,
    }
}

fn stage_to_api(value: MatrixOAuthAcquireStage) -> MatrixOAuthQrStage {
    match value {
        MatrixOAuthAcquireStage::Starting => MatrixOAuthQrStage::Starting,
        MatrixOAuthAcquireStage::QrReady => MatrixOAuthQrStage::QrReady,
        MatrixOAuthAcquireStage::CheckCodeToDisplay => MatrixOAuthQrStage::CheckCodeToDisplay,
        MatrixOAuthAcquireStage::CheckCodeRequired => MatrixOAuthQrStage::CheckCodeRequired,
        MatrixOAuthAcquireStage::WaitingForAuthorization => {
            MatrixOAuthQrStage::WaitingForAuthorization
        }
        MatrixOAuthAcquireStage::SyncingSecrets => MatrixOAuthQrStage::SyncingSecrets,
        MatrixOAuthAcquireStage::Done => MatrixOAuthQrStage::Done,
        MatrixOAuthAcquireStage::Failed => MatrixOAuthQrStage::Failed,
        MatrixOAuthAcquireStage::Cancelled => MatrixOAuthQrStage::Cancelled,
    }
}

fn state_to_api(value: MatrixOAuthAcquireState) -> MatrixOAuthQrFlowDto {
    MatrixOAuthQrFlowDto {
        flow_id: value.flow_id,
        expected_user_id: value.expected_user_id,
        presentation: presentation_to_api(value.presentation),
        stage: stage_to_api(value.stage),
        qr_code_data: value.qr_code_data,
        check_code: value.check_code,
        authorization_user_code: value.authorization_user_code,
        verification_uri: value.verification_uri,
        account_id: value.account_id,
        error_code: value.error_code,
    }
}

fn map_error(value: MatrixOAuthAcquireError) -> MatrixOAuthQrError {
    match value {
        MatrixOAuthAcquireError::InvalidUserId => MatrixOAuthQrError::InvalidRequest(
            "expected_user_id is not a valid Matrix user ID".to_owned(),
        ),
        MatrixOAuthAcquireError::InvalidInput(message) => {
            MatrixOAuthQrError::InvalidRequest(message.to_owned())
        }
        MatrixOAuthAcquireError::Capacity => MatrixOAuthQrError::TooMany(
            "too many Matrix OAuth QR login flows are active or retained; retry later".to_owned(),
        ),
        MatrixOAuthAcquireError::AccountAlreadyActive => {
            MatrixOAuthQrError::Conflict("this Matrix account is already logged in".to_owned())
        }
        MatrixOAuthAcquireError::AccountBeingDeleted => MatrixOAuthQrError::Conflict(
            "this Matrix account is currently being deleted".to_owned(),
        ),
        MatrixOAuthAcquireError::AccountDraining => MatrixOAuthQrError::Conflict(
            "this Matrix account is still shutting down; retry shortly".to_owned(),
        ),
        MatrixOAuthAcquireError::FlowAlreadyExists => MatrixOAuthQrError::Conflict(
            "a Matrix OAuth QR login is already in progress for this Matrix user".to_owned(),
        ),
        MatrixOAuthAcquireError::WrongStage => MatrixOAuthQrError::Conflict(
            "input does not match the flow presentation or current stage".to_owned(),
        ),
        MatrixOAuthAcquireError::NotFound(flow_id) => {
            MatrixOAuthQrError::NotFound(format!("Matrix OAuth QR login flow {flow_id} not found"))
        }
        MatrixOAuthAcquireError::Internal => MatrixOAuthQrError::Internal,
    }
}

#[async_trait]
impl MatrixOAuthQrAcquireService for MatrixOAuthAcquireAdapter {
    async fn create(
        &self,
        expected_user_id: &str,
        presentation: MatrixOAuthQrPresentation,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        self.0
            .create(expected_user_id, presentation_to_sync(presentation))
            .await
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn get(&self, flow_id: Uuid) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        self.0.get(flow_id).map(state_to_api).map_err(map_error)
    }

    async fn submit_scan(
        &self,
        flow_id: Uuid,
        qr_code_data: &str,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        self.0
            .submit_scan(flow_id, qr_code_data)
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn submit_check_code(
        &self,
        flow_id: Uuid,
        check_code: &str,
    ) -> Result<MatrixOAuthQrFlowDto, MatrixOAuthQrError> {
        self.0
            .submit_check_code(flow_id, check_code)
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn cancel(&self, flow_id: Uuid) -> Result<(), MatrixOAuthQrError> {
        self.0.cancel(flow_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sync_stage_has_a_stable_api_stage() {
        let cases = [
            (
                MatrixOAuthAcquireStage::Starting,
                MatrixOAuthQrStage::Starting,
            ),
            (
                MatrixOAuthAcquireStage::QrReady,
                MatrixOAuthQrStage::QrReady,
            ),
            (
                MatrixOAuthAcquireStage::CheckCodeToDisplay,
                MatrixOAuthQrStage::CheckCodeToDisplay,
            ),
            (
                MatrixOAuthAcquireStage::CheckCodeRequired,
                MatrixOAuthQrStage::CheckCodeRequired,
            ),
            (
                MatrixOAuthAcquireStage::WaitingForAuthorization,
                MatrixOAuthQrStage::WaitingForAuthorization,
            ),
            (
                MatrixOAuthAcquireStage::SyncingSecrets,
                MatrixOAuthQrStage::SyncingSecrets,
            ),
            (MatrixOAuthAcquireStage::Done, MatrixOAuthQrStage::Done),
            (MatrixOAuthAcquireStage::Failed, MatrixOAuthQrStage::Failed),
            (
                MatrixOAuthAcquireStage::Cancelled,
                MatrixOAuthQrStage::Cancelled,
            ),
        ];
        for (sync, api) in cases {
            assert_eq!(stage_to_api(sync), api);
        }
    }

    #[test]
    fn both_presentations_map_in_both_directions() {
        for api in [
            MatrixOAuthQrPresentation::Display,
            MatrixOAuthQrPresentation::Scan,
        ] {
            assert_eq!(presentation_to_api(presentation_to_sync(api)), api);
        }
    }

    #[test]
    fn login_conflicts_have_distinct_api_messages() {
        let cases = [
            (
                MatrixOAuthAcquireError::AccountAlreadyActive,
                "this Matrix account is already logged in",
            ),
            (
                MatrixOAuthAcquireError::AccountBeingDeleted,
                "this Matrix account is currently being deleted",
            ),
            (
                MatrixOAuthAcquireError::AccountDraining,
                "this Matrix account is still shutting down; retry shortly",
            ),
            (
                MatrixOAuthAcquireError::FlowAlreadyExists,
                "a Matrix OAuth QR login is already in progress for this Matrix user",
            ),
        ];

        for (error, expected_message) in cases {
            match map_error(error) {
                MatrixOAuthQrError::Conflict(message) => {
                    assert_eq!(message, expected_message);
                }
                other => panic!("expected conflict, got {other:?}"),
            }
        }
    }
}
