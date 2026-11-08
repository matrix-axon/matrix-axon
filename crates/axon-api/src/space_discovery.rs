//! Discovered summaries of unjoined space children and the crawl that finds
//! them (ADR 0128 decisions 1-5): the wire shapes `GET …/space/hierarchy`
//! adds, and `GET …/discovery` of one selected room, built from the store's
//! rows.
//!
//! Discovered fields are never merged into synced fields (ADR 0111): a
//! client shows a child's synced fields when it is joined, and its
//! `discovered` fields when it is not and `discovered.status` is
//! `available`.

use axon_core::JobActivity;
use axon_store::{DiscoveredRow, RoomSummarySnapshot, SpaceCrawlSnapshot};
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use crate::room_details::{DetailRefreshDto, RefreshFailure, RefreshState};

/// What the homeserver last said about a discovered room.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryStatus {
    /// The summary fields below are the latest answer.
    Available,
    /// A complete crawl of a linking space no longer returned the room, so
    /// the account has probably lost access to it.
    NotReturned,
    /// The homeserver refused a summary of the room (403).
    Forbidden,
    /// The homeserver does not know the room (404 `M_NOT_FOUND`).
    NotFound,
}

/// Which homeserver read produced a discovery row.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// `GET /_matrix/client/v1/rooms/{roomId}/hierarchy` of a linking space.
    Hierarchy,
    /// `GET /_matrix/client/v1/room_summary/{roomId}`.
    Summary,
}

/// A summary of an unjoined room from the account's homeserver (ADR 0128
/// decision 4), with its provenance. Every summary field is `null` when it
/// was missing from the latest answer, failed validation (then named in
/// `withheld`), or `status` is not `available`.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct DiscoveredRoomDto {
    pub status: DiscoveryStatus,
    pub source: DiscoverySource,
    /// Unix ms of the answer, from the server clock.
    pub observed_at: i64,
    /// Unix ms when the answer goes stale; a past value means stale.
    pub expires_at: i64,
    /// Summary fields the latest answer carried that failed validation and
    /// were withheld: a name, topic, or alias over its length limit, an avatar
    /// that is not an `mxc://` URI, or a value of the wrong type.
    pub withheld: Vec<String>,
    pub name: Option<String>,
    pub topic: Option<String>,
    /// An `mxc://` URI.
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub num_joined_members: Option<i64>,
    pub join_rule: Option<String>,
    /// The room's `m.room.create` `type`, such as `m.space`.
    pub room_type: Option<String>,
    pub world_readable: Option<bool>,
    pub guest_can_join: Option<bool>,
    /// The room's encryption algorithm. `null` means unknown, never
    /// unencrypted: homeservers before Matrix v1.15 do not report it.
    pub encryption: Option<String>,
    pub room_version: Option<String>,
}

impl DiscoveredRoomDto {
    /// The DTO for a store row, or `None` for a row with an unknown status
    /// or source.
    pub fn from_row(row: DiscoveredRow) -> Option<Self> {
        let status = match row.status.as_str() {
            "available" => DiscoveryStatus::Available,
            "not_returned" => DiscoveryStatus::NotReturned,
            "forbidden" => DiscoveryStatus::Forbidden,
            "not_found" => DiscoveryStatus::NotFound,
            _ => return None,
        };
        let source = match row.source.as_str() {
            "hierarchy" => DiscoverySource::Hierarchy,
            "summary" => DiscoverySource::Summary,
            _ => return None,
        };
        // Only an available row has fields; the store enforces it, and the
        // read repeats it rather than trusting a stray value.
        let summary = row
            .summary
            .filter(|_| status == DiscoveryStatus::Available)
            .unwrap_or(Value::Null);
        let text = |key: &str| summary.get(key).and_then(Value::as_str).map(str::to_owned);
        let flag = |key: &str| summary.get(key).and_then(Value::as_bool);
        Some(Self {
            status,
            source,
            observed_at: row.observed_at,
            expires_at: row.expires_at,
            withheld: if status == DiscoveryStatus::Available {
                row.withheld
            } else {
                Vec::new()
            },
            name: text("name"),
            topic: text("topic"),
            avatar_url: text("avatar_url"),
            canonical_alias: text("canonical_alias"),
            num_joined_members: summary.get("num_joined_members").and_then(Value::as_i64),
            join_rule: text("join_rule"),
            room_type: text("room_type"),
            world_readable: flag("world_readable"),
            guest_can_join: flag("guest_can_join"),
            encryption: text("encryption"),
            room_version: text("room_version"),
        })
    }
}

/// How a space's last crawl ended, or that it is still going.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrawlOutcome {
    /// Pages remain; the next `enrich` read resumes the crawl if no page is
    /// queued or running.
    InProgress,
    /// Every page was read.
    Complete,
    /// The crawl stopped at a limit; `failure` says which, or is `null` when
    /// it reached the page limit.
    Truncated,
    /// A request failed; `failure` says why and `next_attempt_at` when the
    /// crawl may resume.
    Failed,
}

/// Whether the account's homeserver serves the hierarchy endpoint.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrawlSupport {
    /// No crawl has had an answer yet.
    Unknown,
    Supported,
    /// The endpoint is missing; no crawl is attempted until
    /// `next_attempt_at`.
    Unsupported,
}

/// Why a crawl failed or stopped early. Allowlisted; never upstream text.
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrawlFailure {
    Timeout,
    ServerError,
    RateLimited,
    Transport,
    Unauthorized,
    /// An unexpected status or a malformed body.
    InvalidResponse,
    /// The homeserver refused to list the space (403).
    Forbidden,
    /// The homeserver does not know the space (404 `M_NOT_FOUND`).
    NotFound,
    /// One room's entry alone exceeded the page size limit.
    TooLarge,
    /// The homeserver handed back a pagination token it had already given.
    PaginationLoop,
    /// The homeserver's pagination session expired twice in one crawl.
    CursorExpired,
}

impl CrawlFailure {
    fn parse(category: &str) -> Option<Self> {
        Some(match category {
            "timeout" => Self::Timeout,
            "server_error" => Self::ServerError,
            "rate_limited" => Self::RateLimited,
            "transport" => Self::Transport,
            "unauthorized" => Self::Unauthorized,
            "invalid_response" => Self::InvalidResponse,
            "forbidden" => Self::Forbidden,
            "not_found" => Self::NotFound,
            "too_large" => Self::TooLarge,
            "pagination_loop" => Self::PaginationLoop,
            "cursor_expired" => Self::CursorExpired,
            _ => return None,
        })
    }
}

/// A space's discovery crawl (ADR 0128 decision 3). `state` lives in memory
/// only, so after a restart it is `idle`; everything else persists.
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq, Eq)]
pub struct SpaceDiscoveryDto {
    /// Whether a page is waiting for, or using, the account's worker.
    pub state: RefreshState,
    /// `null` when the space has never been crawled.
    pub outcome: Option<CrawlOutcome>,
    /// Pages the current crawl has committed.
    pub pages: i32,
    /// The rooms the crawl asks for per page; halved for the rest of a crawl
    /// after a page over Axon's size limit. `null` before the first crawl.
    pub page_limit: Option<i32>,
    /// Children the current crawl has returned so far.
    pub rooms_cached: i32,
    /// Unix ms when the current crawl started from its first page.
    pub started_at: Option<i64>,
    /// Unix ms when the crawl ended `complete` or `truncated`.
    pub completed_at: Option<i64>,
    /// Unix ms until which an ended crawl, a denial, or `unsupported` stands;
    /// an `enrich` read after it starts a new crawl.
    pub expires_at: Option<i64>,
    pub support: CrawlSupport,
    /// The last failure, or the limit a `truncated` crawl stopped at.
    pub failure: Option<CrawlFailure>,
    /// Unix ms before which a failed crawl is not resumed.
    pub next_attempt_at: Option<i64>,
}

impl SpaceDiscoveryDto {
    /// The crawl status of `snapshot`, with the worker's `activity` for the
    /// space's crawl.
    pub fn new(snapshot: &SpaceCrawlSnapshot, activity: JobActivity) -> Self {
        let state = match activity {
            JobActivity::Idle => RefreshState::Idle,
            JobActivity::Queued => RefreshState::Queued,
            JobActivity::Running => RefreshState::Running,
        };
        let Some(crawl) = snapshot.crawl.as_ref() else {
            return Self {
                state,
                outcome: None,
                pages: 0,
                page_limit: None,
                rooms_cached: 0,
                started_at: None,
                completed_at: None,
                expires_at: None,
                support: CrawlSupport::Unknown,
                failure: None,
                next_attempt_at: None,
            };
        };
        Self {
            state,
            outcome: match crawl.outcome.as_str() {
                "in_progress" => Some(CrawlOutcome::InProgress),
                "complete" => Some(CrawlOutcome::Complete),
                "truncated" => Some(CrawlOutcome::Truncated),
                "failed" => Some(CrawlOutcome::Failed),
                _ => None,
            },
            pages: crawl.pages,
            page_limit: Some(crawl.page_limit),
            rooms_cached: crawl.rooms_cached,
            started_at: Some(crawl.started_at),
            completed_at: crawl.completed_at,
            expires_at: crawl.expires_at,
            support: match crawl.support.as_str() {
                "supported" => CrawlSupport::Supported,
                "unsupported" => CrawlSupport::Unsupported,
                _ => CrawlSupport::Unknown,
            },
            failure: crawl.failure.as_deref().and_then(CrawlFailure::parse),
            next_attempt_at: crawl.next_attempt_at,
        }
    }
}

/// Whether the account's homeserver serves room summaries for this room
/// (ADR 0128 decision 5).
#[derive(Debug, Clone, Copy, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummarySupport {
    /// No summary request has had an answer yet.
    Unknown,
    Supported,
    /// The homeserver advertises neither Matrix v1.15 nor MSC3266's
    /// `im.nheko.summary`, or answered that it lacks the endpoint. No
    /// summary is requested until `refresh.next_attempt_at`; meanwhile the
    /// room's summary comes only from a crawl of a joined space that links
    /// it, which this read schedules.
    Unsupported,
}

/// What Axon knows about one unjoined room from the account's homeserver, and
/// where its room-summary refresh stands (ADR 0128 decision 5).
#[derive(Debug, Clone, Serialize, ToSchema, PartialEq)]
pub struct RoomDiscoveryDto {
    pub room_id: String,
    /// Axon may ask the homeserver about this room: the account has not
    /// joined it, and a space the account has joined links it with a current
    /// `m.space.child`. Any other room is never requested.
    pub eligible: bool,
    /// The cached summary, from a room-summary request or a crawl of a
    /// linking space, or `null` when Axon has none. Always `null` for a
    /// joined room, whose synced state is authoritative.
    pub discovered: Option<DiscoveredRoomDto>,
    pub support: SummarySupport,
    /// The room-summary refresh. `state` is the worker's activity while a
    /// summary is due, and `idle` otherwise; `next_attempt_at` is the end of
    /// a failure backoff, or when an `unsupported` endpoint is checked again.
    pub refresh: DetailRefreshDto,
}

impl RoomDiscoveryDto {
    /// The DTO for `room_id`'s `snapshot`, with the worker's `activity` for
    /// its summary job. As in ADR 0126, a due summary has no attempt time:
    /// the read has just asked for it.
    pub fn new(room_id: String, snapshot: RoomSummarySnapshot, activity: JobActivity) -> Self {
        let refresh = snapshot.refresh.as_ref();
        let support = match refresh.map(|r| r.support.as_str()) {
            Some("supported") => SummarySupport::Supported,
            Some("unsupported") => SummarySupport::Unsupported,
            _ => SummarySupport::Unknown,
        };
        let state = if snapshot.due {
            match activity {
                JobActivity::Idle => RefreshState::Idle,
                JobActivity::Queued => RefreshState::Queued,
                JobActivity::Running => RefreshState::Running,
            }
        } else {
            RefreshState::Idle
        };
        Self {
            room_id,
            eligible: snapshot.eligible,
            discovered: snapshot
                .discovered
                .filter(|_| !snapshot.joined)
                .and_then(DiscoveredRoomDto::from_row),
            support,
            refresh: DetailRefreshDto {
                state,
                failure: refresh
                    .and_then(|r| r.failure.as_deref())
                    .and_then(RefreshFailure::parse),
                next_attempt_at: refresh
                    .and_then(|r| r.next_attempt_at)
                    .filter(|_| !snapshot.due),
            },
        }
    }
}
