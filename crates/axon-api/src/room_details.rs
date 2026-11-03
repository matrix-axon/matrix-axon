//! Homeserver-acquired room details and absence confirmations (ADR 0126
//! decisions 2-4): the acquisition port, and the refresh shapes both reads
//! share.
//!
//! Reads never wait for the homeserver. They return what is cached and, when
//! a detail is unknown or expired and no backoff is in force, ask the
//! account's acquisition worker for a refresh through
//! [`RoomDetailAcquirer`]. Only rooms the account has joined are eligible.

use axon_core::{JobActivity, RoomDetailJob};
use axon_store::DetailRow;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

/// Port to each account's acquisition worker. Both calls are in memory and
/// never wait.
pub trait RoomDetailAcquirer: Send + Sync {
    /// Ask for `job` on `room_id`. Ignored when the account's run is not
    /// active; the next read asks again.
    fn request(&self, account_id: Uuid, room_id: &str, job: RoomDetailJob);
    /// Where `job` on `room_id` stands.
    fn activity(&self, account_id: Uuid, room_id: &str, job: RoomDetailJob) -> JobActivity;
}

/// Fallback when the binary injects no acquirer (API tests): nothing is
/// acquired and every job is idle.
pub(crate) struct NoRoomDetailAcquisition;

impl RoomDetailAcquirer for NoRoomDetailAcquisition {
    fn request(&self, _: Uuid, _: &str, _: RoomDetailJob) {}

    fn activity(&self, _: Uuid, _: &str, _: RoomDetailJob) -> JobActivity {
        JobActivity::Idle
    }
}

/// Whether a refresh is waiting for, or using, the account's worker. In
/// memory only: after a restart every refresh is `idle`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefreshState {
    Idle,
    /// Waiting in the account's queue, including for a homeserver gate permit;
    /// waiting is never a failure.
    Queued,
    Running,
}

/// Why the last refresh attempt failed. Allowlisted; never upstream text.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefreshFailure {
    Timeout,
    ServerError,
    RateLimited,
    Transport,
    Unauthorized,
    /// An unexpected status or a malformed body.
    InvalidResponse,
}

impl RefreshFailure {
    fn parse(category: &str) -> Option<Self> {
        Some(match category {
            "timeout" => Self::Timeout,
            "server_error" => Self::ServerError,
            "rate_limited" => Self::RateLimited,
            "transport" => Self::Transport,
            "unauthorized" => Self::Unauthorized,
            "invalid_response" => Self::InvalidResponse,
            _ => return None,
        })
    }
}

/// The state of attempts to refresh a cached value, separate from the value
/// itself, so a failed refresh leaves usable stale data visible.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct DetailRefreshDto {
    pub state: RefreshState,
    /// The last attempt's failure; `null` after a conclusive answer.
    pub failure: Option<RefreshFailure>,
    /// Unix ms before which no refresh is attempted, when one is set.
    pub next_attempt_at: Option<i64>,
}

impl DetailRefreshDto {
    /// A refresh for `row`. The worker's activity applies only while the row
    /// is due: a fresh or backed-off row is not what a queued job will ask.
    /// `not_before` is when the row next becomes due, if it is not due.
    fn new(row: &DetailRow, activity: JobActivity, not_before: Option<i64>) -> Self {
        Self {
            state: if row.due {
                state(activity)
            } else {
                RefreshState::Idle
            },
            failure: row.failure.as_deref().and_then(RefreshFailure::parse),
            next_attempt_at: if row.due { None } else { not_before },
        }
    }
}

fn state(activity: JobActivity) -> RefreshState {
    match activity {
        JobActivity::Idle => RefreshState::Idle,
        JobActivity::Queued => RefreshState::Queued,
        JobActivity::Running => RefreshState::Running,
    }
}

/// The absence fields of one metadata snapshot with no tuple (ADR 0126
/// decision 2): when the homeserver last confirmed the type absent, and the
/// state of confirming it. `row` is the type's acquisition row; `activity` is
/// the room's absence job.
pub(crate) fn absence_fields(
    row: Option<&DetailRow>,
    activity: JobActivity,
) -> (Option<i64>, Option<DetailRefreshDto>) {
    let Some(row) = row else {
        // Never attempted: `null`, unless a job is about to make the first
        // attempt.
        let check = (activity != JobActivity::Idle).then(|| DetailRefreshDto {
            state: state(activity),
            failure: None,
            next_attempt_at: None,
        });
        return (None, check);
    };
    let confirmed = row.value_status.as_deref() == Some("absent");
    // A denial or a coverage-gap answer holds the type until it expires;
    // a client reads that as "failed or denied". A confirmation's own
    // expiry is just its re-confirmation.
    let not_before = if confirmed {
        row.next_attempt_at
    } else {
        row.expires_at.max(row.next_attempt_at)
    };
    (
        confirmed.then_some(row.fetched_at).flatten(),
        Some(DetailRefreshDto::new(row, activity, not_before)),
    )
}

/// What Axon last learned about a homeserver detail.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HomeserverDetailStatus {
    /// Never answered (or not eligible: only joined rooms are acquired).
    Unknown,
    Available,
    /// Some entries failed validation and were withheld.
    Partial,
    /// The answer exceeded Axon's limits and was withheld, not cut.
    TooLarge,
    /// The homeserver refused (403).
    Forbidden,
    /// The homeserver does not know the room (404 `M_NOT_FOUND`).
    NotFound,
    /// The homeserver does not implement the endpoint.
    Unsupported,
}

impl HomeserverDetailStatus {
    fn parse(status: Option<&str>) -> Self {
        match status {
            Some("available") => Self::Available,
            Some("partial") => Self::Partial,
            Some("too_large") => Self::TooLarge,
            Some("forbidden") => Self::Forbidden,
            Some("not_found") => Self::NotFound,
            Some("unsupported") => Self::Unsupported,
            _ => Self::Unknown,
        }
    }
}

/// Whether the account's homeserver publishes the room in its directory.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryVisibility {
    Public,
    Private,
}

/// The aliases the account's homeserver maintains for the room (`GET
/// /rooms/{roomId}/aliases`). Distinct from the advertised aliases in
/// `m.room.canonical_alias`, which `GET …/metadata` serves.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct LocalAliasesDetailDto {
    pub status: HomeserverDetailStatus,
    /// `null` unless status is `available` or `partial`.
    pub aliases: Option<Vec<String>>,
    /// Unix ms of the last conclusive answer, from the server clock.
    pub fetched_at: Option<i64>,
    /// Unix ms when that answer goes stale; a past value means stale.
    pub expires_at: Option<i64>,
    pub refresh: DetailRefreshDto,
}

/// The room's publication in the account's homeserver directory (`GET
/// /directory/list/room/{roomId}`).
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct DirectoryVisibilityDetailDto {
    pub status: HomeserverDetailStatus,
    /// `null` unless status is `available`.
    pub visibility: Option<DirectoryVisibility>,
    /// Unix ms of the last conclusive answer, from the server clock.
    pub fetched_at: Option<i64>,
    /// Unix ms when that answer goes stale; a past value means stale.
    pub expires_at: Option<i64>,
    pub refresh: DetailRefreshDto,
}

/// Room details only the account's homeserver knows, from Axon's cache.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct RoomHomeserverDetailsDto {
    pub local_aliases: LocalAliasesDetailDto,
    pub directory_visibility: DirectoryVisibilityDetailDto,
}

/// The cached value, timing, and refresh of one homeserver detail.
fn detail_parts(
    row: Option<&DetailRow>,
    activity: JobActivity,
) -> (
    HomeserverDetailStatus,
    Option<i64>,
    Option<i64>,
    DetailRefreshDto,
) {
    match row {
        None => (
            HomeserverDetailStatus::Unknown,
            None,
            None,
            DetailRefreshDto {
                state: state(activity),
                failure: None,
                next_attempt_at: None,
            },
        ),
        Some(row) => (
            HomeserverDetailStatus::parse(row.value_status.as_deref()),
            row.fetched_at,
            row.expires_at,
            DetailRefreshDto::new(row, activity, row.next_attempt_at),
        ),
    }
}

pub(crate) fn local_aliases(
    row: Option<&DetailRow>,
    activity: JobActivity,
) -> LocalAliasesDetailDto {
    let (status, fetched_at, expires_at, refresh) = detail_parts(row, activity);
    let aliases = matches!(
        status,
        HomeserverDetailStatus::Available | HomeserverDetailStatus::Partial
    )
    .then(|| row.and_then(|row| serde_json::from_value(row.value.clone()?).ok()))
    .flatten();
    LocalAliasesDetailDto {
        status: if aliases.is_none()
            && matches!(
                status,
                HomeserverDetailStatus::Available | HomeserverDetailStatus::Partial
            ) {
            HomeserverDetailStatus::Unknown
        } else {
            status
        },
        aliases,
        fetched_at,
        expires_at,
        refresh,
    }
}

pub(crate) fn directory_visibility(
    row: Option<&DetailRow>,
    activity: JobActivity,
) -> DirectoryVisibilityDetailDto {
    let (status, fetched_at, expires_at, refresh) = detail_parts(row, activity);
    let visibility = (status == HomeserverDetailStatus::Available)
        .then(|| match row?.value.as_ref()?.get("visibility")?.as_str()? {
            "public" => Some(DirectoryVisibility::Public),
            "private" => Some(DirectoryVisibility::Private),
            _ => None,
        })
        .flatten();
    DirectoryVisibilityDetailDto {
        status: if visibility.is_none() && status == HomeserverDetailStatus::Available {
            HomeserverDetailStatus::Unknown
        } else {
            status
        },
        visibility,
        fetched_at,
        expires_at,
        refresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(value_status: Option<&str>, due: bool) -> DetailRow {
        DetailRow {
            kind: "absence".into(),
            state_type: "m.room.encryption".into(),
            value_status: value_status.map(str::to_owned),
            value: None,
            fetched_at: value_status.map(|_| 1_000),
            expires_at: value_status.map(|_| 9_000),
            failure: None,
            next_attempt_at: None,
            due,
        }
    }

    #[test]
    fn absence_fields_tell_pending_from_failed_from_confirmed() {
        // Never attempted and nothing coming.
        assert_eq!(absence_fields(None, JobActivity::Idle), (None, None));
        // Never attempted, first attempt queued.
        let (confirmed, check) = absence_fields(None, JobActivity::Queued);
        assert_eq!(confirmed, None);
        assert_eq!(check.unwrap().state, RefreshState::Queued);
        // Confirmed, and fresh: the worker's activity does not apply.
        let (confirmed, check) =
            absence_fields(Some(&row(Some("absent"), false)), JobActivity::Running);
        assert_eq!(confirmed, Some(1_000));
        assert_eq!(
            check.unwrap(),
            DetailRefreshDto {
                state: RefreshState::Idle,
                failure: None,
                next_attempt_at: None
            }
        );
        // Denied: idle with a future attempt time.
        let (confirmed, check) =
            absence_fields(Some(&row(Some("forbidden"), false)), JobActivity::Idle);
        assert_eq!(confirmed, None);
        assert_eq!(check.unwrap().next_attempt_at, Some(9_000));
        // Failed and backing off.
        let mut failed = row(None, false);
        failed.failure = Some("timeout".into());
        failed.next_attempt_at = Some(5_000);
        let check = absence_fields(Some(&failed), JobActivity::Queued)
            .1
            .unwrap();
        assert_eq!(
            check,
            DetailRefreshDto {
                state: RefreshState::Idle,
                failure: Some(RefreshFailure::Timeout),
                next_attempt_at: Some(5_000)
            }
        );
        // An expired confirmation being re-confirmed stays visible.
        let (confirmed, check) =
            absence_fields(Some(&row(Some("absent"), true)), JobActivity::Running);
        assert_eq!(confirmed, Some(1_000));
        assert_eq!(check.unwrap().state, RefreshState::Running);
    }

    #[test]
    fn homeserver_details_keep_stale_values_beside_failures() {
        let mut aliases = row(Some("partial"), true);
        aliases.kind = "local_aliases".into();
        aliases.state_type = String::new();
        aliases.value = Some(json!(["#a:example.org"]));
        aliases.failure = Some("server_error".into());
        let dto = local_aliases(Some(&aliases), JobActivity::Queued);
        assert_eq!(dto.status, HomeserverDetailStatus::Partial);
        assert_eq!(
            dto.aliases.as_deref(),
            Some(&["#a:example.org".to_owned()][..])
        );
        assert_eq!((dto.fetched_at, dto.expires_at), (Some(1_000), Some(9_000)));
        assert_eq!(
            dto.refresh,
            DetailRefreshDto {
                state: RefreshState::Queued,
                failure: Some(RefreshFailure::ServerError),
                next_attempt_at: None
            }
        );

        let unknown = directory_visibility(None, JobActivity::Idle);
        assert_eq!(
            (unknown.status, unknown.visibility, unknown.refresh.state),
            (HomeserverDetailStatus::Unknown, None, RefreshState::Idle)
        );
        let mut public = row(Some("available"), false);
        public.value = Some(json!({"visibility": "public"}));
        assert_eq!(
            directory_visibility(Some(&public), JobActivity::Idle).visibility,
            Some(DirectoryVisibility::Public)
        );
        let denied = directory_visibility(Some(&row(Some("forbidden"), false)), JobActivity::Idle);
        assert_eq!(
            (denied.status, denied.visibility),
            (HomeserverDetailStatus::Forbidden, None)
        );
    }
}
