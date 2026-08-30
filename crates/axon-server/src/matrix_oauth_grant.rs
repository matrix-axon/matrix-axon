//! Composition-root adapter for account-scoped Matrix OAuth QR grants.

use async_trait::async_trait;
use axon_api::{
    MatrixOAuthQrGrantError, MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantService,
    MatrixOAuthQrPresentation, MatrixOAuthQrStage,
};
use axon_sync::{
    MatrixOAuthGrantEngine, MatrixOAuthGrantError, MatrixOAuthGrantPresentation,
    MatrixOAuthGrantStage, MatrixOAuthGrantState,
};
use uuid::Uuid;

pub struct MatrixOAuthGrantAdapter(pub MatrixOAuthGrantEngine);

fn presentation_to_sync(value: MatrixOAuthQrPresentation) -> MatrixOAuthGrantPresentation {
    match value {
        MatrixOAuthQrPresentation::Display => MatrixOAuthGrantPresentation::Display,
        MatrixOAuthQrPresentation::Scan => MatrixOAuthGrantPresentation::Scan,
    }
}

fn presentation_to_api(value: MatrixOAuthGrantPresentation) -> MatrixOAuthQrPresentation {
    match value {
        MatrixOAuthGrantPresentation::Display => MatrixOAuthQrPresentation::Display,
        MatrixOAuthGrantPresentation::Scan => MatrixOAuthQrPresentation::Scan,
    }
}

fn stage_to_api(value: MatrixOAuthGrantStage) -> MatrixOAuthQrStage {
    match value {
        MatrixOAuthGrantStage::Starting => MatrixOAuthQrStage::Starting,
        MatrixOAuthGrantStage::QrReady => MatrixOAuthQrStage::QrReady,
        MatrixOAuthGrantStage::CheckCodeToDisplay => MatrixOAuthQrStage::CheckCodeToDisplay,
        MatrixOAuthGrantStage::CheckCodeRequired => MatrixOAuthQrStage::CheckCodeRequired,
        MatrixOAuthGrantStage::WaitingForAuthorization => {
            MatrixOAuthQrStage::WaitingForAuthorization
        }
        MatrixOAuthGrantStage::SyncingSecrets => MatrixOAuthQrStage::SyncingSecrets,
        MatrixOAuthGrantStage::Done => MatrixOAuthQrStage::Done,
        MatrixOAuthGrantStage::Failed => MatrixOAuthQrStage::Failed,
        MatrixOAuthGrantStage::Cancelled => MatrixOAuthQrStage::Cancelled,
    }
}

fn state_to_api(value: MatrixOAuthGrantState) -> MatrixOAuthQrGrantFlowDto {
    MatrixOAuthQrGrantFlowDto {
        flow_id: value.flow_id,
        account_id: value.account_id,
        presentation: presentation_to_api(value.presentation),
        stage: stage_to_api(value.stage),
        qr_code_data: value.qr_code_data,
        check_code: value.check_code,
        verification_uri: value.verification_uri,
        error_code: value.error_code,
    }
}

fn map_error(value: MatrixOAuthGrantError) -> MatrixOAuthQrGrantError {
    match value {
        MatrixOAuthGrantError::AccountNotFound(account_id) => {
            MatrixOAuthQrGrantError::NotFound(format!("Matrix account {account_id} not found"))
        }
        MatrixOAuthGrantError::AccountNotActive(_) => {
            MatrixOAuthQrGrantError::Conflict("the Matrix account is not active".to_owned())
        }
        MatrixOAuthGrantError::AccountBeingDeleted(_) => {
            MatrixOAuthQrGrantError::Conflict("the Matrix account is being deleted".to_owned())
        }
        MatrixOAuthGrantError::DeviceNotTrusted => MatrixOAuthQrGrantError::Conflict(
            "the Matrix account's current device is not trusted".to_owned(),
        ),
        MatrixOAuthGrantError::SecretsUnavailable => MatrixOAuthQrGrantError::Conflict(
            "the Matrix account cannot export the required encryption secrets".to_owned(),
        ),
        MatrixOAuthGrantError::FlowAlreadyExists => MatrixOAuthQrGrantError::Conflict(
            "a Matrix OAuth QR grant is already in progress for this account".to_owned(),
        ),
        MatrixOAuthGrantError::Capacity => MatrixOAuthQrGrantError::TooMany(
            "too many Matrix OAuth QR grant flows are active or retained; retry later".to_owned(),
        ),
        MatrixOAuthGrantError::NotFound(flow_id) => MatrixOAuthQrGrantError::NotFound(format!(
            "Matrix OAuth QR grant flow {flow_id} not found"
        )),
        MatrixOAuthGrantError::WrongStage => MatrixOAuthQrGrantError::Conflict(
            "input does not match the flow presentation or current stage".to_owned(),
        ),
        MatrixOAuthGrantError::InvalidInput(message) => {
            MatrixOAuthQrGrantError::InvalidRequest(message.to_owned())
        }
        MatrixOAuthGrantError::Unavailable => MatrixOAuthQrGrantError::Unavailable(
            "the Matrix account client is temporarily unavailable; retry shortly".to_owned(),
        ),
        MatrixOAuthGrantError::Internal => MatrixOAuthQrGrantError::Internal,
    }
}

#[async_trait]
impl MatrixOAuthQrGrantService for MatrixOAuthGrantAdapter {
    async fn create(
        &self,
        account_id: Uuid,
        presentation: MatrixOAuthQrPresentation,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        self.0
            .create(account_id, presentation_to_sync(presentation))
            .await
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn get(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        self.0
            .get(account_id, flow_id)
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn submit_scan(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
        qr_code_data: &str,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        self.0
            .submit_scan(account_id, flow_id, qr_code_data)
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn submit_check_code(
        &self,
        account_id: Uuid,
        flow_id: Uuid,
        check_code: &str,
    ) -> Result<MatrixOAuthQrGrantFlowDto, MatrixOAuthQrGrantError> {
        self.0
            .submit_check_code(account_id, flow_id, check_code)
            .map(state_to_api)
            .map_err(map_error)
    }

    async fn cancel(&self, account_id: Uuid, flow_id: Uuid) -> Result<(), MatrixOAuthQrGrantError> {
        self.0.cancel(account_id, flow_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grant_stage_maps_to_the_stable_api_stage() {
        let cases = [
            (
                MatrixOAuthGrantStage::Starting,
                MatrixOAuthQrStage::Starting,
            ),
            (MatrixOAuthGrantStage::QrReady, MatrixOAuthQrStage::QrReady),
            (
                MatrixOAuthGrantStage::CheckCodeToDisplay,
                MatrixOAuthQrStage::CheckCodeToDisplay,
            ),
            (
                MatrixOAuthGrantStage::CheckCodeRequired,
                MatrixOAuthQrStage::CheckCodeRequired,
            ),
            (
                MatrixOAuthGrantStage::WaitingForAuthorization,
                MatrixOAuthQrStage::WaitingForAuthorization,
            ),
            (
                MatrixOAuthGrantStage::SyncingSecrets,
                MatrixOAuthQrStage::SyncingSecrets,
            ),
            (MatrixOAuthGrantStage::Done, MatrixOAuthQrStage::Done),
            (MatrixOAuthGrantStage::Failed, MatrixOAuthQrStage::Failed),
            (
                MatrixOAuthGrantStage::Cancelled,
                MatrixOAuthQrStage::Cancelled,
            ),
        ];
        for (sync, api) in cases {
            assert_eq!(stage_to_api(sync), api);
        }
    }

    #[test]
    fn trust_and_export_failures_are_safe_conflicts() {
        for error in [
            MatrixOAuthGrantError::DeviceNotTrusted,
            MatrixOAuthGrantError::SecretsUnavailable,
        ] {
            assert!(matches!(
                map_error(error),
                MatrixOAuthQrGrantError::Conflict(_)
            ));
        }
    }
}
