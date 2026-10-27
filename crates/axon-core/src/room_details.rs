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
}
