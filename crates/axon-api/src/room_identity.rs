//! Room-member identity warnings and account-scoped acknowledgements.
use async_trait::async_trait;
use axon_core::{IdentityChangeKind, RoomIdentityChange};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::response::ApiError;

/// The action a warning requires.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IdentityChangeKindDto {
    PinViolation,
    VerificationViolation,
}

/// A room member whose current cross-signing identity needs review.
#[derive(Debug, Serialize, ToSchema)]
pub struct RoomIdentityChangeDto {
    pub user_id: String,
    pub kind: IdentityChangeKindDto,
    /// Echo this value in the acknowledgement so it applies to the identity reviewed.
    #[schema(pattern = r"^[0-9a-f]{64}$")]
    pub fingerprint: String,
}

impl From<RoomIdentityChange> for RoomIdentityChangeDto {
    fn from(change: RoomIdentityChange) -> Self {
        Self {
            user_id: change.user_id,
            kind: match change.kind {
                IdentityChangeKind::PinViolation => IdentityChangeKindDto::PinViolation,
                IdentityChangeKind::VerificationViolation => {
                    IdentityChangeKindDto::VerificationViolation
                }
            },
            fingerprint: change.fingerprint,
        }
    }
}

/// The identity presented when the client showed the warning.
#[derive(Debug, Deserialize, ToSchema)]
pub struct ResolveIdentityChange {
    /// The fingerprint from the room's current `identity_changes` entry.
    #[schema(pattern = r"^[0-9a-f]{64}$")]
    pub fingerprint: String,
}

#[async_trait]
pub trait RoomIdentityService: Send + Sync {
    async fn warnings(&self, account_id: Uuid, room_id: &str) -> Option<Vec<RoomIdentityChange>>;

    async fn resolve(
        &self,
        account_id: Uuid,
        user_id: &str,
        fingerprint: &str,
        kind: IdentityChangeKind,
    ) -> Result<(), ApiError>;
}
