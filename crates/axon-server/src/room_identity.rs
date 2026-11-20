//! Bind the sync engine's room-member identity warnings to the API.
use async_trait::async_trait;
use axon_api::{ApiError, RoomIdentityService};
use axon_core::{IdentityChangeKind, RoomIdentityChange};
use axon_sync::{RoomIdentityEngine, RoomIdentityError};
use uuid::Uuid;

pub struct RoomIdentityAdapter(pub RoomIdentityEngine);

#[async_trait]
impl RoomIdentityService for RoomIdentityAdapter {
    async fn warnings(&self, account_id: Uuid, room_id: &str) -> Option<Vec<RoomIdentityChange>> {
        self.0.warnings(account_id, room_id).await
    }

    async fn resolve(
        &self,
        account_id: Uuid,
        user_id: &str,
        fingerprint: &str,
        kind: IdentityChangeKind,
    ) -> Result<(), ApiError> {
        self.0
            .resolve(account_id, user_id, fingerprint, kind)
            .await
            .map_err(|error| {
                let message = error.to_string();
                match error {
                    RoomIdentityError::InvalidInput => ApiError::bad_request(message),
                    RoomIdentityError::NotFound => ApiError::not_found(message),
                    RoomIdentityError::NotReady | RoomIdentityError::Conflict => {
                        ApiError::conflict(message)
                    }
                    RoomIdentityError::Internal => ApiError::internal(),
                }
            })
    }
}
