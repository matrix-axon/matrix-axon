//! Typed, local-only room detail snapshots. These describe cached state, not
//! live homeserver permissions or discovery results (tracking issue #618).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use axon_store::RoomMetadataStateRow;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Availability of a particular cached state tuple. An unknown tuple is not
/// evidence that a setting is unset, encryption is disabled, or access is public.
#[derive(Debug, Serialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoomMetadataStatus {
    /// Axon has no cached row. It may not have requested/hydrated this type.
    Unknown,
    /// Typed content is available, including explicit empty values/removals.
    Available,
    /// Valid fields remain available; invalid fields or entries are identified.
    Partial,
    /// A row exists but its content was not retained.
    /// An empty object is not classified as redacted: stored state alone does
    /// not always carry enough evidence to establish redaction.
    Unavailable,
    /// Stored content has the wrong shape for this typed snapshot.
    Invalid,
    /// Stored content exceeded the 128 KiB per-state content limit.
    TooLarge,
}

/// A typed snapshot from Axon's account-scoped `room_state` cache. Nullable
/// fields in content preserve omitted values rather than synthesizing Matrix
/// defaults. Defaults, if needed for display, apply only to available content.
/// Event timestamps identify the event; they do not establish when sync last
/// succeeded or when Axon last checked upstream availability.
#[derive(Debug, Serialize, ToSchema)]
pub struct CachedRoomMetadata<T: ToSchema> {
    pub status: RoomMetadataStatus,
    pub event_id: Option<String>,
    pub sender: Option<String>,
    pub origin_ts: Option<i64>,
    /// Paths of malformed fields/entries, with wildcards for list/map entries.
    /// These are shape diagnostics, not permission or redaction information.
    pub invalid_fields: Vec<String>,
    /// `null` unless status is `available` or `partial`.
    #[schema(schema_with = nullable_metadata_content::<T>)]
    pub content: Option<T>,
}

// utoipa's generic inline schema does not preserve Option<T>'s nullability.
// Spell out the union so generated clients accept unknown/unavailable content.
fn nullable_metadata_content<T: ToSchema>() -> utoipa::openapi::RefOr<utoipa::openapi::Schema> {
    use utoipa::openapi::schema::{ObjectBuilder, OneOfBuilder, Type};
    OneOfBuilder::new()
        .item(ObjectBuilder::new().schema_type(Type::Null))
        .item(T::schema())
        .into()
}

impl<T: ToSchema> Default for CachedRoomMetadata<T> {
    fn default() -> Self {
        Self {
            status: RoomMetadataStatus::Unknown,
            event_id: None,
            sender: None,
            origin_ts: None,
            content: None,
            invalid_fields: Vec::new(),
        }
    }
}

impl<T: MetadataContent> CachedRoomMetadata<T> {
    fn from_row(account_id: Uuid, row: RoomMetadataStateRow) -> Self {
        let mut snapshot = Self {
            status: RoomMetadataStatus::Unavailable,
            event_id: Some(row.state.event_id.clone()),
            sender: Some(row.state.sender),
            origin_ts: Some(row.state.origin_ts),
            content: None,
            invalid_fields: Vec::new(),
        };
        if row.oversized {
            snapshot.status = RoomMetadataStatus::TooLarge;
        } else if let Some(content) = row.state.content {
            if let Value::Object(mut fields) = content {
                let mut invalid = BTreeSet::new();
                T::repair(&mut fields, &mut invalid);
                snapshot.invalid_fields = invalid.into_iter().collect();
                // All known malformed values have been removed explicitly.
                // Preserve an invalid status if the top-level typed shape is
                // still impossible rather than substituting defaults.
                match serde_json::from_value(Value::Object(fields)) {
                    Ok(content) => {
                        snapshot.status = if snapshot.invalid_fields.is_empty() {
                            RoomMetadataStatus::Available
                        } else {
                            RoomMetadataStatus::Partial
                        };
                        snapshot.content = Some(content);
                    }
                    Err(_) => snapshot.status = RoomMetadataStatus::Invalid,
                }
            } else {
                snapshot.status = RoomMetadataStatus::Invalid;
            }
        }

        if matches!(
            snapshot.status,
            RoomMetadataStatus::Invalid
                | RoomMetadataStatus::Partial
                | RoomMetadataStatus::TooLarge
        ) {
            // Do not log serde's error: it may quote upstream field values.
            tracing::debug!(
                %account_id, room_id = %row.state.room_id,
                event_id = %row.state.event_id, event_type = %row.state.event_type,
                status = ?snapshot.status, "validated room metadata content"
            );
        }
        snapshot
    }
}

pub(crate) trait MetadataContent: DeserializeOwned + ToSchema {
    fn repair(fields: &mut Map<String, Value>, invalid: &mut BTreeSet<String>);
}

fn repair_field<T: DeserializeOwned>(
    fields: &mut Map<String, Value>,
    key: &str,
    invalid: &mut BTreeSet<String>,
) {
    if fields.get(key).is_some_and(|value| {
        !value.is_null() && serde_json::from_value::<T>(value.clone()).is_err()
    }) {
        fields.remove(key);
        invalid.insert(key.into());
    }
}

fn repair_list<T: DeserializeOwned>(
    fields: &mut Map<String, Value>,
    key: &str,
    invalid: &mut BTreeSet<String>,
) {
    if let Some(Value::Array(entries)) = fields.get_mut(key) {
        entries.retain(|value| {
            let valid = serde_json::from_value::<T>(value.clone()).is_ok();
            if !valid {
                invalid.insert(format!("{key}[]"));
            }
            valid
        });
    } else {
        repair_field::<Vec<T>>(fields, key, invalid);
    }
}

fn repair_power_map(fields: &mut Map<String, Value>, key: &str, invalid: &mut BTreeSet<String>) {
    if let Some(Value::Object(entries)) = fields.get_mut(key) {
        entries.retain(|_, value| {
            let valid = serde_json::from_value::<StoredPowerLevel>(value.clone())
                .and_then(StoredPowerLevel::into_integer::<serde_json::Error>)
                .is_ok();
            if !valid {
                invalid.insert(format!("{key}.*"));
            }
            valid
        });
    } else {
        repair_field::<BTreeMap<String, StoredPowerLevel>>(fields, key, invalid);
    }
}

// Field validators share the same types as decoding, including legacy numeric
// handling, instead of maintaining a second permissive interpretation.
macro_rules! metadata_fields {
    ($content:ty { $($key:literal: $kind:ident $(<$value:ty>)?),* $(,)? }) => {
        impl MetadataContent for $content {
            fn repair(fields: &mut Map<String, Value>, invalid: &mut BTreeSet<String>) {
                $($kind $(::<$value>)?(fields, $key, invalid);)*
            }
        }
    };
}

/// Advertised aliases from `m.room.canonical_alias`, not an exhaustive
/// federation-wide alias list or the account homeserver's alias directory.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomAliasesMetadata {
    pub alias: Option<String>,
    pub alt_aliases: Option<Vec<String>>,
}

/// Creation details as stored, including version-dependent creator fields.
/// For versions where `creator` is omitted, the create event's sender is
/// available in the enclosing snapshot. No creator/room-version inference is
/// made from missing state or version-dependent omitted fields.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomCreationMetadata {
    pub creator: Option<String>,
    pub additional_creators: Option<Vec<String>>,
    pub room_version: Option<String>,
    #[serde(rename(deserialize = "m.federate"))]
    pub federate: Option<bool>,
    #[serde(rename(deserialize = "type"))]
    pub room_type: Option<String>,
    pub predecessor: Option<RoomPredecessorMetadata>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomPredecessorMetadata {
    pub room_id: String,
    /// Deprecated since Matrix v1.16; room upgrades may omit this field.
    pub event_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomJoinRulesMetadata {
    pub join_rule: Option<String>,
    /// Restriction conditions, preserving namespaced/unknown condition types.
    pub allow: Option<Vec<RoomJoinConditionMetadata>>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomJoinConditionMetadata {
    #[serde(rename = "type")]
    pub condition_type: String,
    /// Present for `m.room_membership`; other condition types may omit it.
    pub room_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomEncryptionMetadata {
    pub algorithm: Option<String>,
    pub rotation_period_ms: Option<u64>,
    pub rotation_period_msgs: Option<u64>,
}

// Older room versions permit numeric strings in power-level state. Normalize
// those for this typed read without treating arbitrary strings as integers.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredPowerLevel {
    Integer(i64),
    LegacyString(String),
    LegacyFloat(f64),
}

impl StoredPowerLevel {
    fn into_integer<E: serde::de::Error>(self) -> Result<i64, E> {
        let value = match self {
            Self::Integer(value) => value,
            Self::LegacyString(value) => value
                .trim()
                .parse::<i64>()
                .map_err(|_| E::custom("invalid power level"))?,
            Self::LegacyFloat(value) => {
                // Older versions truncate floats toward zero. Apply the same
                // safe-integer bound before casting to avoid saturating casts.
                let value = value.trunc();
                if !value.is_finite()
                    || !(-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&value)
                {
                    return Err(E::custom("power level out of range"));
                }
                value as i64
            }
        };
        // Matrix integers must be exactly representable in JavaScript.
        if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(&value) {
            return Err(E::custom("power level out of range"));
        }
        Ok(value)
    }
}

fn optional_power_level<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    Option::<StoredPowerLevel>::deserialize(deserializer)?
        .map(StoredPowerLevel::into_integer)
        .transpose()
}

fn optional_power_level_map<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, i64>>, D::Error> {
    Option::<BTreeMap<String, StoredPowerLevel>>::deserialize(deserializer)?
        .map(|values| {
            values
                .into_iter()
                .map(|(key, value)| value.into_integer().map(|value| (key, value)))
                .collect()
        })
        .transpose()
}

/// Configured values, not resolved permissions. Omitted values retain their
/// omission; room-version rules (including v12 creator privileges) and Matrix
/// defaults must be applied before these are used for permission decisions.
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomPowerLevelsMetadata {
    #[serde(default, deserialize_with = "optional_power_level")]
    pub ban: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level")]
    pub invite: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level")]
    pub kick: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level")]
    pub redact: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level")]
    pub events_default: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level")]
    pub state_default: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level")]
    pub users_default: Option<i64>,
    #[serde(default, deserialize_with = "optional_power_level_map")]
    pub users: Option<BTreeMap<String, i64>>,
    #[serde(default, deserialize_with = "optional_power_level_map")]
    pub events: Option<BTreeMap<String, i64>>,
    pub notifications: Option<RoomNotificationLevelsMetadata>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomNotificationLevelsMetadata {
    #[serde(default, deserialize_with = "optional_power_level")]
    pub room: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomServerAclMetadata {
    pub allow_ip_literals: Option<bool>,
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomHistoryVisibilityMetadata {
    pub history_visibility: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct RoomGuestAccessMetadata {
    pub guest_access: Option<String>,
}

metadata_fields!(RoomAliasesMetadata {
    "alias": repair_field<String>,
    "alt_aliases": repair_list<String>,
});
metadata_fields!(RoomCreationMetadata {
    "creator": repair_field<String>,
    "additional_creators": repair_list<String>,
    "room_version": repair_field<String>,
    "m.federate": repair_field<bool>,
    "type": repair_field<String>,
    "predecessor": repair_field<RoomPredecessorMetadata>,
});
metadata_fields!(RoomJoinRulesMetadata {
    "join_rule": repair_field<String>,
    "allow": repair_list<RoomJoinConditionMetadata>,
});
metadata_fields!(RoomEncryptionMetadata {
    "algorithm": repair_field<String>,
    "rotation_period_ms": repair_field<u64>,
    "rotation_period_msgs": repair_field<u64>,
});
metadata_fields!(RoomServerAclMetadata {
    "allow_ip_literals": repair_field<bool>,
    "allow": repair_list<String>,
    "deny": repair_list<String>,
});
metadata_fields!(RoomHistoryVisibilityMetadata {
    "history_visibility": repair_field<String>,
});
metadata_fields!(RoomGuestAccessMetadata {
    "guest_access": repair_field<String>,
});

fn repair_power_field(fields: &mut Map<String, Value>, key: &str, invalid: &mut BTreeSet<String>) {
    if fields.get(key).is_some_and(|value| {
        !value.is_null()
            && serde_json::from_value::<StoredPowerLevel>(value.clone())
                .and_then(StoredPowerLevel::into_integer::<serde_json::Error>)
                .is_err()
    }) {
        fields.remove(key);
        invalid.insert(key.into());
    }
}

impl MetadataContent for RoomPowerLevelsMetadata {
    fn repair(fields: &mut Map<String, Value>, invalid: &mut BTreeSet<String>) {
        for key in [
            "ban",
            "invite",
            "kick",
            "redact",
            "events_default",
            "state_default",
            "users_default",
        ] {
            repair_power_field(fields, key, invalid);
        }
        for key in ["users", "events"] {
            repair_power_map(fields, key, invalid);
        }
        if let Some(Value::Object(notifications)) = fields.get_mut("notifications") {
            let mut nested = BTreeSet::new();
            repair_power_field(notifications, "room", &mut nested);
            invalid.extend(
                nested
                    .into_iter()
                    .map(|path| format!("notifications.{path}")),
            );
        } else {
            repair_field::<RoomNotificationLevelsMetadata>(fields, "notifications", invalid);
        }
    }
}

// Define the DTO fields, selected state types, and dispatch together so a new
// snapshot cannot silently be omitted from the query or dropped by a row cap.
macro_rules! room_metadata {
    ($($field:ident: $content:ty => $event_type:literal),+ $(,)?) => {
        /// Bounded local state detail read; no remote acquisition or aggregation.
        #[derive(Debug, Default, Serialize, ToSchema)]
        pub struct RoomMetadataDto {
            $(pub $field: CachedRoomMetadata<$content>,)+
        }

        impl RoomMetadataDto {
            pub(crate) const EVENT_TYPES: &'static [&'static str] = &[$($event_type,)+];

            pub(crate) fn from_rows(account_id: Uuid, rows: Vec<RoomMetadataStateRow>) -> Self {
                let mut metadata = Self::default();
                for row in rows {
                    match row.state.event_type.as_str() {
                        $($event_type => metadata.$field = CachedRoomMetadata::from_row(account_id, row),)+
                        _ => {}
                    }
                }
                metadata
            }
        }
    };
}

room_metadata! {
    aliases: RoomAliasesMetadata => "m.room.canonical_alias",
    creation: RoomCreationMetadata => "m.room.create",
    join_rules: RoomJoinRulesMetadata => "m.room.join_rules",
    encryption: RoomEncryptionMetadata => "m.room.encryption",
    power_levels: RoomPowerLevelsMetadata => "m.room.power_levels",
    server_acl: RoomServerAclMetadata => "m.room.server_acl",
    history_visibility: RoomHistoryVisibilityMetadata => "m.room.history_visibility",
    guest_access: RoomGuestAccessMetadata => "m.room.guest_access",
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_store::RoomStateRow;
    use serde_json::json;

    fn snapshot(event_type: &str, content: Value) -> Value {
        let metadata = RoomMetadataDto::from_rows(
            Uuid::new_v4(),
            vec![RoomMetadataStateRow {
                state: RoomStateRow {
                    room_id: "!test:example.org".into(),
                    event_type: event_type.into(),
                    state_key: String::new(),
                    event_id: "$state".into(),
                    sender: "@creator:example.org".into(),
                    origin_ts: 1,
                    content: Some(content),
                },
                oversized: false,
            }],
        );
        serde_json::to_value(metadata).unwrap()
    }

    #[test]
    fn malformed_optional_fields_preserve_valid_siblings_and_entries() {
        let cases = [
            (
                "m.room.encryption",
                "encryption",
                json!({"algorithm": "m.megolm.v1.aes-sha2", "rotation_period_msgs": -1}),
                "rotation_period_msgs",
            ),
            (
                "m.room.create",
                "creation",
                json!({"room_version": "12", "predecessor": {"event_id": "$old"}}),
                "predecessor",
            ),
            (
                "m.room.join_rules",
                "join_rules",
                json!({"join_rule": "restricted", "allow": [{"type": "m.room_membership", "room_id": "!allowed:example.org"}, {}]}),
                "allow[]",
            ),
            (
                "m.room.server_acl",
                "server_acl",
                json!({"allow": ["*"], "deny": ["bad.example.org", 42, false]}),
                "deny[]",
            ),
        ];
        for (event_type, key, content, invalid) in cases {
            let data = snapshot(event_type, content);
            assert_eq!(data[key]["status"], "partial");
            assert_eq!(data[key]["invalid_fields"], json!([invalid]));
            assert!(data[key]["content"].is_object());
            assert_eq!(data[key]["event_id"], "$state");
        }
        let data = snapshot(
            "m.room.power_levels",
            json!({
                "ban": 50,
                "users": {"@valid:example.org": 50.9, "@invalid:example.org": null},
                "events": {"m.room.name": "100", "m.room.topic": "bad"},
                "notifications": {"room": "bad"}
            }),
        );
        let levels = &data["power_levels"];
        assert_eq!(levels["status"], "partial");
        assert_eq!(
            levels["invalid_fields"],
            json!(["events.*", "notifications.room", "users.*"])
        );
        assert_eq!(levels["content"]["ban"], 50);
        assert_eq!(
            levels["content"]["users"],
            json!({"@valid:example.org": 50})
        );
        assert_eq!(levels["content"]["events"], json!({"m.room.name": 100}));
        assert_eq!(levels["content"]["notifications"]["room"], Value::Null);
    }

    #[test]
    fn selected_state_types_all_dispatch_and_empty_content_is_not_a_redaction_flag() {
        for event_type in RoomMetadataDto::EVENT_TYPES {
            let data = snapshot(event_type, json!({}));
            let values = data.as_object().unwrap();
            assert_eq!(
                values
                    .values()
                    .filter(|s| s["status"] == "available")
                    .count(),
                1
            );
            assert!(values.values().all(|s| s["invalid_fields"] == json!([])));
            let invalid = snapshot(event_type, json!([]));
            assert_eq!(
                invalid
                    .as_object()
                    .unwrap()
                    .values()
                    .filter(|s| s["status"] == "invalid")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn legacy_power_levels_follow_matrix_decimal_and_float_rules() {
        let levels: RoomPowerLevelsMetadata = serde_json::from_value(json!({
            "ban": 50.9,
            "kick": -50.9,
            "users_default": 0.0,
            "users": {"@alice:example.org": 5.114698E4},
            "events": {"m.room.name": " +000100 "},
            "notifications": {"room": "50"}
        }))
        .unwrap();
        assert_eq!(levels.ban, Some(50));
        assert_eq!(levels.kick, Some(-50));
        assert_eq!(levels.users_default, Some(0));
        assert_eq!(levels.users.unwrap()["@alice:example.org"], 51146);
        assert_eq!(levels.events.unwrap()["m.room.name"], 100);
        assert_eq!(levels.notifications.unwrap().room, Some(50));
        for value in [json!(1e30), json!("1_000"), json!("50.0"), json!(true)] {
            assert!(
                serde_json::from_value::<RoomPowerLevelsMetadata>(json!({"ban": value})).is_err()
            );
        }
    }

    #[test]
    fn creation_snapshot_accepts_predecessor_without_event_id() {
        let metadata = RoomMetadataDto::from_rows(
            Uuid::new_v4(),
            vec![RoomMetadataStateRow {
                state: RoomStateRow {
                    room_id: "!replacement:example.org".into(),
                    event_type: "m.room.create".into(),
                    state_key: String::new(),
                    event_id: "$create".into(),
                    sender: "@creator:example.org".into(),
                    origin_ts: 1,
                    content: Some(json!({
                        "room_version": "12",
                        "predecessor": {"room_id": "!previous:example.org"}
                    })),
                },
                oversized: false,
            }],
        );
        assert!(matches!(
            metadata.creation.status,
            RoomMetadataStatus::Available
        ));
        let creation = metadata.creation.content.unwrap();
        assert_eq!(creation.room_version.as_deref(), Some("12"));
        let predecessor = creation.predecessor.unwrap();
        assert_eq!(predecessor.room_id, "!previous:example.org");
        assert_eq!(predecessor.event_id, None);
    }
}
