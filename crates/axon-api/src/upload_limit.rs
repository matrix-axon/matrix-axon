//! Effective media upload limit port (ADR 0143 section 6, issue #1210), for
//! `AccountDto.media_upload_limit` and upload staging.
//!
//! A client can upload only as much as both Axon's staging cap
//! (`media.max_upload_bytes`) and the homeserver's `m.upload.size` allow. The
//! binary injects a provider over the sync engine's cached homeserver limit.
//! Like [`crate::sync_state`], reads are synchronous and never wait on the
//! network, so they cannot pin an account list.

use uuid::Uuid;

/// One account's effective upload limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadLimit {
    /// The largest upload Axon stages for this account: the lower of its own
    /// cap and the homeserver's, or its own cap alone when the homeserver's
    /// is unknown.
    pub max_bytes: u64,
    /// The homeserver's `m.upload.size` as the account's Matrix client can
    /// use it, or `None` when it is unknown (no fresh answer, a failed or
    /// unsupported lookup, or no connected client).
    pub homeserver_max_bytes: Option<u64>,
}

impl UploadLimit {
    /// The limit for Axon's own cap `local_max_bytes` and the homeserver's
    /// `homeserver_max_bytes`, if known. The lower one wins.
    pub fn effective(local_max_bytes: u64, homeserver_max_bytes: Option<u64>) -> Self {
        Self {
            max_bytes: homeserver_max_bytes.map_or(local_max_bytes, |hs| hs.min(local_max_bytes)),
            homeserver_max_bytes,
        }
    }
}

/// Port exposing each account's effective upload limit.
pub trait UploadLimitProvider: Send + Sync {
    /// The effective limit for `account_id`. Must not block or do network I/O
    /// on the caller's task; a provider may start a background refresh.
    /// `None` means the server does not advertise a limit, and the account
    /// DTO omits the field.
    fn upload_limit(&self, account_id: Uuid) -> Option<UploadLimit>;
}

/// Fallback provider used when the binary injects none (e.g. in API tests):
/// no limit is advertised.
pub(crate) struct NoUploadLimit;

impl UploadLimitProvider for NoUploadLimit {
    fn upload_limit(&self, _account_id: Uuid) -> Option<UploadLimit> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::UploadLimit;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn the_lower_limit_wins_in_both_directions() {
        // The homeserver allows more than Axon stages.
        assert_eq!(
            UploadLimit::effective(100 * MIB, Some(1024 * MIB)),
            UploadLimit {
                max_bytes: 100 * MIB,
                homeserver_max_bytes: Some(1024 * MIB),
            }
        );
        // The homeserver allows less.
        assert_eq!(
            UploadLimit::effective(100 * MIB, Some(50 * MIB)),
            UploadLimit {
                max_bytes: 50 * MIB,
                homeserver_max_bytes: Some(50 * MIB),
            }
        );
        assert_eq!(UploadLimit::effective(10, Some(10)).max_bytes, 10);
    }

    #[test]
    fn an_unknown_homeserver_limit_falls_back_to_axons_own_cap() {
        assert_eq!(
            UploadLimit::effective(100 * MIB, None),
            UploadLimit {
                max_bytes: 100 * MIB,
                homeserver_max_bytes: None,
            }
        );
    }
}
