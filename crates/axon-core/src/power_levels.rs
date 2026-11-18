//! Wire-neutral power-level types, shared by `axon-api`'s `PowerLevelsSender`
//! port and `axon-sync`'s `SdkGateway::set_power_levels`/`power_levels` (ADR
//! 0068, M19e). Like [`CreateRoomRequest`](crate::CreateRoomRequest), these
//! live in this lowest crate so the two sibling crates share one type without
//! depending on each other; the composition root in `axon-server` forwards
//! them verbatim.

use std::collections::HashMap;

/// A requested change to a room's `m.room.power_levels` state event.
/// `None`/absent fields leave that level unchanged; `users` entries are
/// merged into (not replacing) the room's existing per-user map — an
/// omitted user keeps their current level. `room_name`/`room_avatar`/
/// `room_topic` are deliberately not represented here: M19d already owns
/// those levels via its own dedicated field routes, so this type only
/// covers the role-threshold defaults and per-user assignment, the two axes
/// M19e adds.
///
/// `acknowledge_self_demotion` is the guardrail escape hatch (ADR 0068's
/// M19e note): without it, a change that would drop the caller's own
/// resolved power level below what's needed to send another
/// `m.room.power_levels` event is rejected before it reaches the homeserver,
/// since that write would otherwise succeed and permanently strand the
/// caller with no way to self-correct.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PowerLevelChanges {
    pub ban: Option<i64>,
    pub invite: Option<i64>,
    pub kick: Option<i64>,
    pub redact: Option<i64>,
    pub events_default: Option<i64>,
    pub state_default: Option<i64>,
    pub users_default: Option<i64>,
    /// User id -> requested power level. Merged into the room's existing
    /// `users` map; a user not present here keeps their current level.
    pub users: HashMap<String, i64>,
    pub acknowledge_self_demotion: bool,
}

/// A room's fully resolved power levels (defaults filled in), returned by
/// the M19e read path. Mirrors matrix-sdk's own `RoomPowerLevels` shape for
/// the fields Axon exposes; `room_name`/`room_avatar`/`room_topic` are
/// omitted here too, for the same reason as on [`PowerLevelChanges`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPowerLevels {
    pub ban: i64,
    pub invite: i64,
    pub kick: i64,
    pub redact: i64,
    pub events_default: i64,
    pub state_default: i64,
    pub users_default: i64,
    pub users: HashMap<String, i64>,
    /// The room's creators, who hold an infinite level and never appear in
    /// `users` from room version 12 onwards (ADR 0137 decision 4, issue 324).
    /// Sorted, and empty for an earlier room version.
    pub privileged_creators: Vec<String>,
    /// The caller's own level, resolved with the room version's rules, so a
    /// client need not know about creators to gate an edit.
    pub own_level: ResolvedUserLevel,
}

/// One user's resolved power level: an integer, or infinite for a room
/// creator from room version 12 onwards (ADR 0137 decision 4). Mirrors
/// ruma's `UserPowerLevel`, which this crate cannot depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedUserLevel {
    Int(i64),
    Infinite,
}

impl Default for ResolvedUserLevel {
    /// The spec's `users_default` when a room has no power-levels event.
    fn default() -> Self {
        ResolvedUserLevel::Int(0)
    }
}
