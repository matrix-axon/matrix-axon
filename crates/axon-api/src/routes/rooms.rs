//! Room endpoints: the cross-account list and a room's paginated timeline.

use std::collections::HashMap;
use std::sync::Arc;

use axon_store::Store;
use axum::extract::State;
use futures_util::future::{try_join, try_join4};
use serde::Deserialize;
use serde_json::Value;
use utoipa::IntoParams;
use uuid::Uuid;

use crate::cursor;
use crate::dto::{
    EventDto, MemberDto, RoomDto, RoomInfoDto, RoomUpgradeDto, SpaceChildDto, SpaceParentDto,
    ThreadSummaryDto, TimelinePage,
};
use crate::extract::{Path, Query};
use crate::member_profiles::{MemberProfile, MemberProfileService};
use crate::response::{ApiError, ApiResponse};

/// Default timeline page size when `limit` is omitted.
const DEFAULT_LIMIT: i64 = 50;
/// Hard cap on timeline page size, regardless of the requested `limit`.
const MAX_LIMIT: i64 = 200;

/// Query parameters for `GET /v1/rooms`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RoomsQuery {
    /// Narrow the list to a single account. Omit for all accounts.
    pub account_id: Option<Uuid>,
}

/// List rooms across accounts, most-recent-activity first.
#[utoipa::path(
    get,
    path = "/v1/rooms",
    params(RoomsQuery),
    responses(
        (status = 200, description = "Rooms, newest activity first", body = ApiResponse<Vec<RoomDto>>),
    ),
    tag = "rooms",
)]
pub async fn list_rooms(
    State(store): State<Store>,
    Query(q): Query<RoomsQuery>,
) -> Result<ApiResponse<Vec<RoomDto>>, ApiError> {
    let rooms = store.list_rooms(q.account_id).await?;
    Ok(ApiResponse::new(
        rooms.into_iter().map(RoomDto::from).collect(),
    ))
}

/// Query parameters for the timeline read.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TimelineQuery {
    /// Opaque cursor from a previous page's `next_cursor`; omit for the newest page.
    pub cursor: Option<String>,
    /// Page size (default 50, max 200).
    pub limit: Option<i64>,
    /// Jump to a specific point in time: Unix milliseconds. Returns the page of
    /// events at or before this timestamp. Mutually exclusive with `cursor`;
    /// supplying both is a 400.
    pub at_ts: Option<i64>,
}

/// Read a room's timeline, newest first, with cursor pagination.
///
/// An unknown `account_id`/`room_id` yields an empty page (200), not a 404 — an
/// empty timeline and a non-existent room are indistinguishable here and an
/// empty page is the natural answer.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/timeline",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
        TimelineQuery,
    ),
    responses(
        (status = 200, description = "A page of timeline events", body = ApiResponse<TimelinePage>),
        (status = 400, description = "Malformed cursor, or both cursor and at_ts supplied", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn room_timeline(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
    Query(q): Query<TimelineQuery>,
) -> Result<ApiResponse<TimelinePage>, ApiError> {
    let before = match (q.cursor.as_deref(), q.at_ts) {
        (Some(_), Some(_)) => {
            return Err(ApiError::bad_request(
                "cursor and at_ts are mutually exclusive",
            ));
        }
        (Some(c), None) => {
            Some(cursor::decode(c).ok_or_else(|| ApiError::bad_request("invalid cursor"))?)
        }
        (None, Some(ts)) => Some(cursor::from_ts(ts)),
        (None, None) => None,
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let rows = store
        .room_timeline(account_id, &room_id, before, limit)
        .await?;
    // A full page implies there may be more; a short page is the end.
    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|r| cursor::encode(r.cursor())))
        .flatten();
    let events = rows
        .into_iter()
        .map(|r| EventDto::from_row(account_id, r))
        .collect();

    Ok(ApiResponse::new(TimelinePage {
        events,
        next_cursor,
    }))
}

/// List the current members of a room — the resolved `m.room.member` state,
/// one entry per user. Useful for seeding a display-name map without requiring
/// membership events to appear in the loaded timeline. An unknown
/// `account_id`/`room_id` yields an empty list (200), not a 404.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/members",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Current room members", body = ApiResponse<Vec<MemberDto>>),
    ),
    tag = "rooms",
)]
pub async fn room_members(
    State(store): State<Store>,
    State(member_profiles): State<Arc<dyn MemberProfileService>>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<MemberDto>>, ApiError> {
    let rows = store
        .room_state_of_type(account_id, &room_id, "m.room.member")
        .await?;
    let mut members: Vec<MemberDto> = rows.into_iter().map(MemberDto::from_state_row).collect();
    let missing_avatar_user_ids: Vec<String> = members
        .iter()
        .filter(|member| member.avatar_url.is_none())
        .map(|member| member.user_id.clone())
        .collect();

    if !missing_avatar_user_ids.is_empty() {
        match member_profiles
            .profiles(account_id, &room_id, &missing_avatar_user_ids)
            .await
        {
            Ok(profiles) => enrich_member_avatars(&mut members, profiles),
            Err(err) => {
                tracing::warn!(
                    %account_id,
                    %room_id,
                    error = %err,
                    "failed to enrich room member avatars from cached profiles"
                );
            }
        }
    }

    Ok(ApiResponse::new(members))
}

fn enrich_member_avatars(members: &mut [MemberDto], profiles: Vec<MemberProfile>) {
    let avatars: HashMap<String, String> = profiles
        .into_iter()
        .filter_map(|profile| profile.avatar_url.map(|avatar| (profile.user_id, avatar)))
        .collect();
    for member in members {
        if member.avatar_url.is_none() {
            member.avatar_url = avatars.get(&member.user_id).cloned();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(user_id: &str, avatar_url: Option<&str>) -> MemberDto {
        MemberDto {
            user_id: user_id.to_owned(),
            membership: "join".to_owned(),
            display_name: None,
            avatar_url: avatar_url.map(str::to_owned),
        }
    }

    #[test]
    fn enrich_member_avatars_only_fills_missing_values() {
        let mut members = vec![
            member("@alice:localhost", None),
            member("@bob:localhost", Some("mxc://hs/member-bob")),
            member("@carol:localhost", None),
        ];

        enrich_member_avatars(
            &mut members,
            vec![
                MemberProfile {
                    user_id: "@alice:localhost".to_owned(),
                    avatar_url: Some("mxc://hs/profile-alice".to_owned()),
                },
                MemberProfile {
                    user_id: "@bob:localhost".to_owned(),
                    avatar_url: Some("mxc://hs/profile-bob".to_owned()),
                },
                MemberProfile {
                    user_id: "@carol:localhost".to_owned(),
                    avatar_url: None,
                },
            ],
        );

        assert_eq!(
            members[0].avatar_url.as_deref(),
            Some("mxc://hs/profile-alice")
        );
        assert_eq!(
            members[1].avatar_url.as_deref(),
            Some("mxc://hs/member-bob")
        );
        assert_eq!(members[2].avatar_url, None);
    }
}

/// List the threads in a room (M8): one summary per distinct `m.thread` root,
/// most-recently-active first, each with its reply count and latest reply. The
/// root events themselves are read via the single-event endpoint; a thread's
/// members are paged via [`thread_timeline`]. An unknown `account_id`/`room_id`
/// (or a room with no threads) yields an empty list (200), not a 404. The list is
/// hard-capped in the store against a room with a pathological thread count.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/threads",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "Threads, most-recently-active first", body = ApiResponse<Vec<ThreadSummaryDto>>),
    ),
    tag = "rooms",
)]
pub async fn room_threads(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<ThreadSummaryDto>>, ApiError> {
    let threads = store.room_threads(account_id, &room_id).await?;
    Ok(ApiResponse::new(
        threads.into_iter().map(ThreadSummaryDto::from).collect(),
    ))
}

/// Query parameters for a cursor-paginated timeline read: an opaque cursor plus
/// a page size, with no timestamp jump. (The room timeline additionally accepts
/// `at_ts`; see [`TimelineQuery`].)
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TimelineCursorQuery {
    /// Opaque cursor from a previous page's `next_cursor`; omit for the newest page.
    pub cursor: Option<String>,
    /// Page size (default 50, max 200).
    pub limit: Option<i64>,
}

/// Read a thread's timeline (M8): the `m.thread` members whose root is
/// `root_id`, newest first, with the same cursor pagination as the room
/// timeline. The thread root itself is not included (fetch it via the
/// single-event endpoint). Reactions and edits on thread members are aggregated
/// onto their rows like any other timeline read. An unknown thread yields an
/// empty page (200); a malformed cursor is a 400.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/threads/{root_id}/timeline",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
        ("root_id" = String, Path, description = "Matrix event id of the thread root"),
        TimelineCursorQuery,
    ),
    responses(
        (status = 200, description = "A page of thread events", body = ApiResponse<TimelinePage>),
        (status = 400, description = "Malformed cursor", body = crate::response::ErrorResponse),
    ),
    tag = "rooms",
)]
pub async fn thread_timeline(
    State(store): State<Store>,
    Path((account_id, room_id, root_id)): Path<(Uuid, String, String)>,
    Query(q): Query<TimelineCursorQuery>,
) -> Result<ApiResponse<TimelinePage>, ApiError> {
    let before = match q.cursor.as_deref() {
        Some(c) => Some(cursor::decode(c).ok_or_else(|| ApiError::bad_request("invalid cursor"))?),
        None => None,
    };
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let rows = store
        .thread_timeline(account_id, &room_id, &root_id, before, limit)
        .await?;
    // A full page implies there may be more; a short page is the end.
    let next_cursor = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|r| cursor::encode(r.cursor())))
        .flatten();
    let events = rows
        .into_iter()
        .map(|r| EventDto::from_row(account_id, r))
        .collect();

    Ok(ApiResponse::new(TimelinePage {
        events,
        next_cursor,
    }))
}

/// List a space's children (`m.space.child`, MSC1772-ordered: `order` string
/// ascending, absent sorts last, then `origin_ts`, then `room_id`), each
/// enriched with the child room's own cached name/avatar/room_type where Axon
/// knows that room too (issue #404, ADR 0084). An unknown `account_id`/
/// `room_id`, or a room with no children, yields an empty list (200), not a
/// 404. The store caps this at a fixed row count against a room with a
/// pathological number of children (`SPACE_HIERARCHY_CAP` in
/// `axon-store::spaces`).
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/space/children",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id of the space"),
    ),
    responses(
        (status = 200, description = "The space's children, MSC1772-ordered", body = ApiResponse<Vec<SpaceChildDto>>),
    ),
    tag = "rooms",
)]
pub async fn space_children(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<SpaceChildDto>>, ApiError> {
    let rows = store.space_children(account_id, &room_id).await?;
    Ok(ApiResponse::new(
        rows.into_iter().map(SpaceChildDto::from).collect(),
    ))
}

/// List the spaces a room belongs to (`m.space.parent`) — the reverse lookup
/// of [`space_children`]. An unknown `account_id`/`room_id`, or a room with no
/// parents, yields an empty list (200), not a 404. Same row cap as
/// `space_children`.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/space/parents",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "The room's parent spaces", body = ApiResponse<Vec<SpaceParentDto>>),
    ),
    tag = "rooms",
)]
pub async fn space_parents(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<SpaceParentDto>>, ApiError> {
    let rows = store.space_parents(account_id, &room_id).await?;
    Ok(ApiResponse::new(
        rows.into_iter().map(SpaceParentDto::from).collect(),
    ))
}

/// List the room's currently pinned messages (`m.room.pinned_events`),
/// resolved and hydrated exactly like any other timeline read (redaction
/// masking, edit collapse, reaction tally) rather than a raw event-id list
/// (issue #404, ADR 0084). Order follows the pinned list's own array order. An
/// unknown `account_id`/`room_id`, or a room with no pinned state, yields an
/// empty list (200), not a 404.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/pinned",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "The room's pinned events", body = ApiResponse<Vec<EventDto>>),
    ),
    tag = "rooms",
)]
pub async fn room_pinned(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<Vec<EventDto>>, ApiError> {
    let rows = store.pinned_events(account_id, &room_id).await?;
    Ok(ApiResponse::new(
        rows.into_iter()
            .map(|r| EventDto::from_row(account_id, r))
            .collect(),
    ))
}

/// Resolve a single singleton room-state tuple's `content` (`state_key ""`),
/// or `None` if unset — the shared shape behind [`room_info`] and
/// [`room_upgrade`], each of which reads a handful of independent singleton
/// state types for one room rather than a list (issue #404, ADR 0084).
async fn singleton_state(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    event_type: &str,
) -> Result<Option<Value>, ApiError> {
    Ok(store
        .room_state(account_id, room_id, event_type, "")
        .await?
        .and_then(|row| row.content))
}

/// Pull a string field out of a JSON object, or `None` if absent / not a
/// string — shared by every [`singleton_state_field`] call site so the
/// extraction isn't repeated per field (issue #404, ADR 0084).
fn str_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(str::to_owned)
}

/// [`singleton_state`], then pull one top-level string field out of its
/// content — the shape every field in [`room_info`] and the tombstone half of
/// [`room_upgrade`] needs.
async fn singleton_state_field(
    store: &Store,
    account_id: Uuid,
    room_id: &str,
    event_type: &str,
    field: &str,
) -> Result<Option<String>, ApiError> {
    Ok(singleton_state(store, account_id, room_id, event_type)
        .await?
        .and_then(|content| str_field(&content, field)))
}

/// Bundle of small "what kind of room is this" reads: join rule, history
/// visibility, guest access, and encryption algorithm (issue #404, ADR 0084).
/// Each is an independent singleton `room_state` lookup — fanned out
/// concurrently rather than four sequential round trips, since none depends
/// on another's result. An unknown room or unset state reads as `null` for
/// that field, not a 404.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/info",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "The room's info bundle", body = ApiResponse<RoomInfoDto>),
    ),
    tag = "rooms",
)]
pub async fn room_info(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<RoomInfoDto>, ApiError> {
    let (join_rule, history_visibility, guest_access, encryption_algorithm) = try_join4(
        singleton_state_field(
            &store,
            account_id,
            &room_id,
            "m.room.join_rules",
            "join_rule",
        ),
        singleton_state_field(
            &store,
            account_id,
            &room_id,
            "m.room.history_visibility",
            "history_visibility",
        ),
        singleton_state_field(
            &store,
            account_id,
            &room_id,
            "m.room.guest_access",
            "guest_access",
        ),
        singleton_state_field(
            &store,
            account_id,
            &room_id,
            "m.room.encryption",
            "algorithm",
        ),
    )
    .await?;

    let member_counts = store
        .room_member_counts(account_id, &room_id)
        .await?
        .map(|c| crate::dto::RoomMemberCountsDto {
            joined: c.joined,
            invited: c.invited,
            observed_at: c.observed_at,
        });
    Ok(ApiResponse::new(RoomInfoDto {
        member_counts,
        join_rule,
        history_visibility,
        guest_access,
        encryption_algorithm,
    }))
}

/// Where a tombstoned room's replacement lives, and/or where this room was
/// upgraded from (issue #404, ADR 0084). The two reads are independent, so
/// they're fanned out concurrently rather than run one after the other. An
/// unknown room, or one with neither event, reads as all-`null` (200), not a
/// 404. Deliberately not folded into `RoomDto` — see [`RoomUpgradeDto`] for
/// why.
#[utoipa::path(
    get,
    path = "/v1/accounts/{account_id}/rooms/{room_id}/upgrade",
    params(
        ("account_id" = Uuid, Path, description = "Axon account id"),
        ("room_id" = String, Path, description = "Matrix room id"),
    ),
    responses(
        (status = 200, description = "The room's upgrade chain", body = ApiResponse<RoomUpgradeDto>),
    ),
    tag = "rooms",
)]
pub async fn room_upgrade(
    State(store): State<Store>,
    Path((account_id, room_id)): Path<(Uuid, String)>,
) -> Result<ApiResponse<RoomUpgradeDto>, ApiError> {
    let (tombstoned_to, upgraded_from) = try_join(
        singleton_state_field(
            &store,
            account_id,
            &room_id,
            "m.room.tombstone",
            "replacement_room",
        ),
        async {
            Ok(
                singleton_state(&store, account_id, &room_id, "m.room.create")
                    .await?
                    .and_then(|c| c.get("predecessor").and_then(|p| str_field(p, "room_id"))),
            )
        },
    )
    .await?;

    Ok(ApiResponse::new(RoomUpgradeDto {
        tombstoned_to,
        upgraded_from,
    }))
}
