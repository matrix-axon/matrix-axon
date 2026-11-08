//! Room details a client reads per room and re-reads on `room_details.changed`
//! (ADR 0126).
//!
//! Shared by `axon-sync`, which decides when a change happened, and
//! `axon-api`, which serves the typed reads and names the wire values.

/// The singleton (`state_key` `""`) state types behind
/// `GET …/rooms/{room_id}/metadata` (ADR 0111). A change to the stored tuple
/// of any of them is a `metadata` change.
pub const METADATA_STATE_TYPES: [&str; 8] = [
    "m.room.canonical_alias",
    "m.room.create",
    "m.room.join_rules",
    "m.room.encryption",
    "m.room.power_levels",
    "m.room.server_acl",
    "m.room.history_visibility",
    "m.room.guest_access",
];

/// Whether a state tuple is one of [`METADATA_STATE_TYPES`].
pub fn is_metadata_state(event_type: &str, state_key: &str) -> bool {
    state_key.is_empty() && METADATA_STATE_TYPES.contains(&event_type)
}

/// Which read a `room_details.changed` frame tells a client to repeat.
/// The order is the order sources appear on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoomDetailSource {
    /// `GET …/metadata`: a metadata state tuple was inserted, replaced, or
    /// changed its redaction evidence.
    Metadata,
    /// `GET …/info`'s `member_counts`: the persisted pair changed or was
    /// cleared.
    MemberCounts,
    /// `GET …/homeserver_details`: a detail's value, freshness, or refresh
    /// state changed.
    HomeserverDetails,
    /// `GET …/space/hierarchy` of the frame's room, a space (ADR 0128
    /// decision 9): a committed crawl page changed a cached child summary, or
    /// the crawl reached a conclusive state.
    SpaceHierarchy,
    /// `GET …/discovery` of the frame's room, an unjoined room (ADR 0128
    /// decision 9): a room-summary job changed its discovery row, the
    /// summary endpoint's support, or its failure.
    Discovery,
}

/// The order an absence job confirms a room's missing metadata types (ADR
/// 0126 decision 2): encryption first, because a false "unencrypted" is the
/// costliest mistake.
pub const ABSENCE_ORDER: [&str; 8] = [
    "m.room.encryption",
    "m.room.server_acl",
    "m.room.join_rules",
    "m.room.history_visibility",
    "m.room.guest_access",
    "m.room.power_levels",
    "m.room.canonical_alias",
    "m.room.create",
];

/// A job a read asks an account's acquisition worker for (ADR 0126 decision
/// 4). The queue holds at most one of each per room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoomDetailJob {
    /// The homeserver-local aliases.
    LocalAliases,
    /// The room's directory publication.
    DirectoryVisibility,
    /// Confirm every due metadata type that has no tuple.
    Absence,
    /// One page of the space's hierarchy crawl (ADR 0128 decision 3); the
    /// job queues the next page itself.
    Hierarchy,
    /// The room summary of one unjoined room a joined space links (ADR 0128
    /// decision 5).
    Summary,
}

/// Where a job stands in its account's worker. In memory only: a restart
/// leaves every job idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobActivity {
    Idle,
    Queued,
    Running,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_singleton_metadata_tuples_count() {
        for event_type in METADATA_STATE_TYPES {
            assert!(is_metadata_state(event_type, ""));
            assert!(!is_metadata_state(event_type, "x"));
        }
        assert!(!is_metadata_state("m.room.name", ""));
        assert!(!is_metadata_state("m.room.member", ""));
    }

    #[test]
    fn absence_order_covers_every_metadata_type_once() {
        let mut order = ABSENCE_ORDER.to_vec();
        assert_eq!(order[0], "m.room.encryption");
        order.sort_unstable();
        let mut types = METADATA_STATE_TYPES.to_vec();
        types.sort_unstable();
        assert_eq!(order, types);
    }
}
