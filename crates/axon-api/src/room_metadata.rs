//! Typed, local-only room detail snapshots. These describe cached state, not
//! live homeserver permissions or discovery results (tracking issue #618).

use std::collections::HashMap;

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
    /// A row exists but its content was withheld (for example by redaction).
    /// An empty object is not classified as redacted: stored state alone does
    /// not always carry enough evidence to establish redaction.
    Unavailable,
    /// Stored content has the wrong shape for this typed snapshot.
    Invalid,
    /// Stored content exceeded the 64 KiB per-state content limit.
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
    /// `null` unless status is `available`.
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
        }
    }
}

impl<T: DeserializeOwned + ToSchema> CachedRoomMetadata<T> {
    fn from_row(account_id: Uuid, row: RoomMetadataStateRow) -> Self {
        let mut snapshot = Self {
            status: RoomMetadataStatus::Unavailable,
            event_id: Some(row.state.event_id.clone()),
            sender: Some(row.state.sender),
            origin_ts: Some(row.state.origin_ts),
            content: None,
        };
        if row.oversized {
            snapshot.status = RoomMetadataStatus::TooLarge;
        } else if let Some(content) = row.state.content {
            // Deserialize the whole known shape, so a malformed list or entry
            // cannot silently look like an empty list or a valid default.
            match content.is_object().then(|| serde_json::from_value(content)) {
                Some(Ok(content)) => {
                    snapshot.status = RoomMetadataStatus::Available;
                    snapshot.content = Some(content);
                }
                _ => snapshot.status = RoomMetadataStatus::Invalid,
            }
        }
        if matches!(
            snapshot.status,
            RoomMetadataStatus::Invalid | RoomMetadataStatus::TooLarge
        ) {
            // Do not log serde's error: it may quote upstream field values.
            tracing::warn!(
                %account_id, room_id = %row.state.room_id,
                event_id = %row.state.event_id, event_type = %row.state.event_type,
                status = ?snapshot.status, "withheld invalid room metadata content"
            );
        }
        snapshot
    }
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
    pub event_id: String,
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
}

impl StoredPowerLevel {
    fn into_integer<E: serde::de::Error>(self) -> Result<i64, E> {
        let value = match self {
            Self::Integer(value) => value,
            Self::LegacyString(value) => value
                .trim()
                .parse::<i64>()
                .map_err(|_| E::custom("invalid power level"))?,
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
) -> Result<Option<HashMap<String, i64>>, D::Error> {
    Option::<HashMap<String, StoredPowerLevel>>::deserialize(deserializer)?
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
    pub users: Option<HashMap<String, i64>>,
    #[serde(default, deserialize_with = "optional_power_level_map")]
    pub events: Option<HashMap<String, i64>>,
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

/// Fixed-cost detail read from cached room state. It does not fetch upstream,
/// aggregate members, merge discovery results, or change account membership.
/// Unknown account/room IDs return unknown snapshots, like existing state reads.
#[derive(Debug, Default, Serialize, ToSchema)]
pub struct RoomMetadataDto {
    pub aliases: CachedRoomMetadata<RoomAliasesMetadata>,
    pub creation: CachedRoomMetadata<RoomCreationMetadata>,
    pub join_rules: CachedRoomMetadata<RoomJoinRulesMetadata>,
    pub encryption: CachedRoomMetadata<RoomEncryptionMetadata>,
    pub power_levels: CachedRoomMetadata<RoomPowerLevelsMetadata>,
    pub server_acl: CachedRoomMetadata<RoomServerAclMetadata>,
    pub history_visibility: CachedRoomMetadata<RoomHistoryVisibilityMetadata>,
    pub guest_access: CachedRoomMetadata<RoomGuestAccessMetadata>,
}

impl RoomMetadataDto {
    pub(crate) fn from_rows(account_id: Uuid, rows: Vec<RoomMetadataStateRow>) -> Self {
        let mut metadata = Self::default();
        for row in rows {
            match row.state.event_type.as_str() {
                "m.room.canonical_alias" => {
                    metadata.aliases = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.create" => {
                    metadata.creation = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.join_rules" => {
                    metadata.join_rules = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.encryption" => {
                    metadata.encryption = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.power_levels" => {
                    metadata.power_levels = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.server_acl" => {
                    metadata.server_acl = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.history_visibility" => {
                    metadata.history_visibility = CachedRoomMetadata::from_row(account_id, row);
                }
                "m.room.guest_access" => {
                    metadata.guest_access = CachedRoomMetadata::from_row(account_id, row);
                }
                _ => {}
            }
        }
        metadata
    }
}
