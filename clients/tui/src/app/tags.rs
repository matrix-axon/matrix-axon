//! Room-list favourites: `m.favourite` writes, live patches, and one-shot
//! migration of `[display] pinned_rooms` (ADR 0103 / issue #369).

use std::cmp::Ordering;
use std::collections::HashMap;

use uuid::Uuid;

use crate::api::{
    parse_room_tags, room_ids_in_direct_map, AccountDataChangedDto, RoomDto, RoomTag, FAVOURITE_TAG,
};
use crate::config::TuiConfig;

use super::{App, RoomKey, RoomTargetResolution, Status};

/// How far the local-pin → `m.favourite` migration has got this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PinMigration {
    #[default]
    Pending,
    InFlight,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagWriteKind {
    Pin,
    Unpin,
    Migrate,
}

pub(crate) struct TagWriteOutcome {
    kind: TagWriteKind,
    /// Rooms whose tags we changed locally so a failed write can restore them.
    previous_tags: HashMap<RoomKey, Vec<RoomTag>>,
    title: String,
    result: Result<(), String>,
}

/// One `PUT m.favourite` with an explicit order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FavouriteAssignment {
    pub key: RoomKey,
    pub order: f64,
}

/// Below this gap, pin-to-top cannot fit a new order and must rebalance
/// (ADR 0103's drag-reorder threshold, reused for TUI pin-to-top).
const ORDER_EPSILON: f64 = 1e-10;

impl App {
    /// Pin the target room (or re-pin an already-favourite room to the top).
    /// Writes `m.favourite` off the event loop; the list updates immediately
    /// and reverts if the PUT fails.
    pub(crate) fn pin_room(&mut self, target: Option<&str>) {
        let Some(index) = self.resolve_pin_room_index(target) else {
            return;
        };
        if self.tag_write_busy {
            self.status = Status::from("a tag write is already in progress".to_owned());
            return;
        }
        let key = RoomKey::from(&self.rooms.rooms[index]);
        let title = self.rooms.rooms[index].title().to_owned();
        let assignments = pin_to_top_assignments(&self.rooms.rooms, &key);
        let previous_tags = snapshot_tags(&self.rooms.rooms, assignments.iter().map(|a| &a.key));
        apply_favourite_assignments(&mut self.rooms.rooms, &assignments);
        let local_len = self.pinned_rooms.len();
        self.pinned_rooms.retain(|existing| existing != &key);
        if self.pinned_rooms.len() != local_len {
            let _ = self.persist_remaining_local_pins();
        }
        self.resort_rooms();
        self.spawn_tag_write(TagWriteKind::Pin, previous_tags, title, assignments, None);
    }

    /// Unpin the target room. No-op (with a status message) if it is not a
    /// favourite and not a not-yet-migrated local pin.
    pub(crate) fn unpin_room(&mut self, target: Option<&str>) {
        let Some(index) = self.resolve_pin_room_index(target) else {
            return;
        };
        if self.tag_write_busy {
            self.status = Status::from("a tag write is already in progress".to_owned());
            return;
        }
        let key = RoomKey::from(&self.rooms.rooms[index]);
        let title = self.rooms.rooms[index].title().to_owned();
        let was_favourite = self.rooms.rooms[index].is_favourite();
        let was_local = self.pinned_rooms.iter().any(|existing| existing == &key);
        if !was_favourite && !was_local {
            self.status = Status::from(format!("{title} is not pinned"));
            return;
        }
        let local_len = self.pinned_rooms.len();
        self.pinned_rooms.retain(|existing| existing != &key);
        if self.pinned_rooms.len() != local_len {
            let _ = self.persist_remaining_local_pins();
        }
        if !was_favourite {
            self.resort_rooms();
            self.status = Status::from(format!("unpinned {title}"));
            return;
        }
        let previous_tags = snapshot_tags(&self.rooms.rooms, std::iter::once(&key));
        clear_favourite(&mut self.rooms.rooms[index]);
        self.resort_rooms();
        self.spawn_tag_write(
            TagWriteKind::Unpin,
            previous_tags,
            title,
            Vec::new(),
            Some(key),
        );
    }

    /// Resolve the room a `/pin`/`/unpin` request targets: the explicit argument
    /// if given, otherwise the currently selected room. Sets a status message and
    /// returns `None` when resolution fails.
    fn resolve_pin_room_index(&mut self, target: Option<&str>) -> Option<usize> {
        match target {
            Some(target) => match self.resolve_room_target(target) {
                RoomTargetResolution::Match(index) => Some(index),
                RoomTargetResolution::Ambiguous(options) => {
                    self.status =
                        Status::Info(format!("room name is ambiguous: {}", options.join(", ")));
                    None
                }
                RoomTargetResolution::Missing => {
                    self.status = Status::from(format!("room not found: {target}"));
                    None
                }
            },
            None => {
                let index = self.rooms.selected;
                if index.is_none() {
                    self.status = Status::from("select a room to pin".to_owned());
                }
                index
            }
        }
    }

    fn spawn_tag_write(
        &mut self,
        kind: TagWriteKind,
        previous_tags: HashMap<RoomKey, Vec<RoomTag>>,
        title: String,
        assignments: Vec<FavouriteAssignment>,
        delete: Option<RoomKey>,
    ) {
        let Some(tx) = self.tag_write_tx.clone() else {
            // Unit tests have no channel: local tags already applied.
            self.status = match kind {
                TagWriteKind::Pin => Status::from(format!("pinned {title}")),
                TagWriteKind::Unpin => Status::from(format!("unpinned {title}")),
                TagWriteKind::Migrate => Status::from("migrated local pins".to_owned()),
            };
            if kind == TagWriteKind::Migrate {
                self.pinned_rooms.clear();
                self.pin_migration = PinMigration::Done;
            }
            return;
        };
        self.tag_write_busy = true;
        self.status = match kind {
            TagWriteKind::Pin => Status::from(format!("pinning {title}")),
            TagWriteKind::Unpin => Status::from(format!("unpinning {title}")),
            TagWriteKind::Migrate => Status::from("migrating local pins to m.favourite".to_owned()),
        };
        let client = self.client.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(key) = delete {
                    client
                        .remove_room_tag(key.account_id, &key.room_id, FAVOURITE_TAG)
                        .await
                        .map_err(|err| err.to_string())?;
                }
                for assignment in &assignments {
                    client
                        .set_room_tag(
                            assignment.key.account_id,
                            &assignment.key.room_id,
                            FAVOURITE_TAG,
                            Some(assignment.order),
                        )
                        .await
                        .map_err(|err| err.to_string())?;
                }
                Ok(())
            }
            .await;
            let _ = tx.send(TagWriteOutcome {
                kind,
                previous_tags,
                title,
                result,
            });
        });
    }

    pub(crate) async fn handle_tag_write_outcome(&mut self, outcome: TagWriteOutcome) {
        self.tag_write_busy = false;
        match outcome.result {
            Ok(()) => {
                if outcome.kind == TagWriteKind::Migrate {
                    self.pinned_rooms.clear();
                    self.pin_migration = PinMigration::Done;
                    if let Err(err) = self.clear_local_pins_config() {
                        self.status = Status::from(format!(
                            "migrated local pins (config clear failed: {err})"
                        ));
                        return;
                    }
                }
                let refresh_warning = match self.client.list_rooms(self.account_filter).await {
                    Ok(rooms) => {
                        self.apply_room_refresh(rooms);
                        None
                    }
                    Err(err) => Some(err.to_string()),
                };
                self.status = Status::from(match (outcome.kind, refresh_warning) {
                    (TagWriteKind::Pin, None) => format!("pinned {}", outcome.title),
                    (TagWriteKind::Unpin, None) => format!("unpinned {}", outcome.title),
                    (TagWriteKind::Migrate, None) => {
                        "migrated local pins to m.favourite".to_owned()
                    }
                    (TagWriteKind::Pin, Some(err)) => {
                        format!("pinned {} (room refresh failed: {err})", outcome.title)
                    }
                    (TagWriteKind::Unpin, Some(err)) => {
                        format!("unpinned {} (room refresh failed: {err})", outcome.title)
                    }
                    (TagWriteKind::Migrate, Some(err)) => {
                        format!("migrated local pins (room refresh failed: {err})")
                    }
                });
            }
            Err(err) => {
                restore_tags(&mut self.rooms.rooms, &outcome.previous_tags);
                self.resort_rooms();
                if outcome.kind == TagWriteKind::Migrate {
                    self.pin_migration = PinMigration::Pending;
                }
                let verb = match outcome.kind {
                    TagWriteKind::Pin => "pin",
                    TagWriteKind::Unpin => "unpin",
                    TagWriteKind::Migrate => "pin migration",
                };
                self.status = Status::from(format!("{verb} failed: {err}"));
            }
        }
    }

    /// After a room-list refresh, upload leftover `[display] pinned_rooms` or
    /// discard them when the homeserver already has `m.favourite` for that
    /// account (ADR 0103).
    pub(super) fn maybe_start_pin_migration(&mut self) {
        if self.pin_migration != PinMigration::Pending {
            return;
        }
        if self.pinned_rooms.is_empty() {
            self.pin_migration = PinMigration::Done;
            return;
        }
        if self.rooms.rooms.is_empty() {
            return;
        }

        // Capture before any skip so a pin we write this session cannot look
        // like a pre-existing homeserver favourite.
        if self.server_favourite_accounts.is_none() {
            self.server_favourite_accounts = Some(
                self.rooms
                    .rooms
                    .iter()
                    .filter(|room| room.is_favourite())
                    .map(|room| room.account_id)
                    .collect(),
            );
        }

        let mut known_accounts: std::collections::HashSet<Uuid> = self
            .rooms
            .rooms
            .iter()
            .map(|room| room.account_id)
            .collect();
        known_accounts.extend(self.accounts.inactive_ids.iter().copied());
        known_accounts.extend(
            self.accounts
                .accounts
                .iter()
                .map(|account| account.account_id),
        );
        if self
            .pinned_rooms
            .iter()
            .any(|key| !known_accounts.contains(&key.account_id))
        {
            // An account's rooms have not landed yet; try again on the next refresh.
            return;
        }

        let accounts_with_favourite = self
            .server_favourite_accounts
            .clone()
            .expect("captured on the first non-empty room list");
        // Homeserver wins: drop local pins for any account that already had a
        // favourite on the first room list (not one we wrote this session).
        self.pinned_rooms
            .retain(|key| !accounts_with_favourite.contains(&key.account_id));
        self.pinned_rooms
            .retain(|key| !self.accounts.inactive_ids.contains(&key.account_id));

        if self.pinned_rooms.is_empty() {
            self.pin_migration = PinMigration::Done;
            let _ = self.clear_local_pins_config();
            return;
        }

        let assignments = migration_assignments(&self.pinned_rooms);
        let previous_tags = snapshot_tags(&self.rooms.rooms, assignments.iter().map(|a| &a.key));
        apply_favourite_assignments(&mut self.rooms.rooms, &assignments);
        self.resort_rooms();
        self.pin_migration = PinMigration::InFlight;
        self.spawn_tag_write(
            TagWriteKind::Migrate,
            previous_tags,
            String::new(),
            assignments,
            None,
        );
    }

    fn persist_remaining_local_pins(&self) -> Result<(), String> {
        let entries: Vec<String> = self
            .pinned_rooms
            .iter()
            .map(RoomKey::to_config_entry)
            .collect();
        TuiConfig::save_pinned_rooms(&self.config_path, &entries).map_err(|err| err.to_string())
    }

    fn clear_local_pins_config(&self) -> Result<(), String> {
        TuiConfig::save_pinned_rooms(&self.config_path, &[]).map_err(|err| err.to_string())
    }

    /// Apply a live `account_data.changed` frame: patch `tags` or `is_direct`.
    pub(crate) fn apply_account_data_changed(
        &mut self,
        account_id: Uuid,
        payload: AccountDataChangedDto,
    ) {
        match payload.event_type.as_str() {
            "m.tag" => {
                let Some(room_id) = payload.room_id.as_deref() else {
                    return;
                };
                let Some(room) = self
                    .rooms
                    .rooms
                    .iter_mut()
                    .find(|room| room.account_id == account_id && room.room_id == room_id)
                else {
                    return;
                };
                room.tags = parse_room_tags(&payload.content);
                self.resort_rooms();
            }
            "m.direct" => {
                let direct: std::collections::HashSet<&str> =
                    room_ids_in_direct_map(&payload.content).collect();
                for room in &mut self.rooms.rooms {
                    if room.account_id == account_id {
                        room.is_direct = direct.contains(room.room_id.as_str());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Pin-to-top: one PUT with `min/2` (or `0.5` when this is the first
/// favourite). If the current minimum cannot fit a new order, rebalance every
/// favourite as `i / (n + 1)` with the newly pinned room at index 0.
pub(crate) fn pin_to_top_assignments(
    rooms: &[RoomDto],
    target: &RoomKey,
) -> Vec<FavouriteAssignment> {
    let mut others: Vec<&RoomDto> = rooms
        .iter()
        .filter(|room| RoomKey::from(*room) != *target && room.is_favourite())
        .collect();
    others.sort_by(|a, b| compare_favourite_order(a.favourite_order(), b.favourite_order()));
    let min = others.iter().find_map(|room| room.favourite_order());
    match min {
        None => vec![FavouriteAssignment {
            key: target.clone(),
            order: 0.5,
        }],
        Some(min) if min > ORDER_EPSILON => vec![FavouriteAssignment {
            key: target.clone(),
            order: min / 2.0,
        }],
        Some(_) => {
            let n = others.len() + 1;
            let denom = (n + 1) as f64;
            let mut assignments = vec![FavouriteAssignment {
                key: target.clone(),
                order: 0.0,
            }];
            for (index, room) in others.iter().enumerate() {
                assignments.push(FavouriteAssignment {
                    key: RoomKey::from(*room),
                    order: (index + 1) as f64 / denom,
                });
            }
            assignments
        }
    }
}

fn migration_assignments(pinned: &[RoomKey]) -> Vec<FavouriteAssignment> {
    let mut by_account: HashMap<Uuid, Vec<&RoomKey>> = HashMap::new();
    for key in pinned {
        by_account.entry(key.account_id).or_default().push(key);
    }
    let mut assignments = Vec::new();
    for keys in by_account.values() {
        let n = keys.len() as f64;
        for (index, key) in keys.iter().enumerate() {
            assignments.push(FavouriteAssignment {
                key: (*key).clone(),
                order: index as f64 / n,
            });
        }
    }
    assignments
}

fn compare_favourite_order(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (left, right) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn snapshot_tags<'a>(
    rooms: &[RoomDto],
    keys: impl Iterator<Item = &'a RoomKey>,
) -> HashMap<RoomKey, Vec<RoomTag>> {
    let wanted: std::collections::HashSet<RoomKey> = keys.cloned().collect();
    rooms
        .iter()
        .filter(|room| wanted.contains(&RoomKey::from(*room)))
        .map(|room| (RoomKey::from(room), room.tags.clone()))
        .collect()
}

fn apply_favourite_assignments(rooms: &mut [RoomDto], assignments: &[FavouriteAssignment]) {
    for assignment in assignments {
        if let Some(room) = rooms
            .iter_mut()
            .find(|room| RoomKey::from(&**room) == assignment.key)
        {
            upsert_favourite(room, assignment.order);
        }
    }
}

fn upsert_favourite(room: &mut RoomDto, order: f64) {
    if let Some(tag) = room.tags.iter_mut().find(|tag| tag.name == FAVOURITE_TAG) {
        tag.order = Some(order);
    } else {
        room.tags.push(RoomTag {
            name: FAVOURITE_TAG.to_owned(),
            order: Some(order),
        });
    }
}

fn clear_favourite(room: &mut RoomDto) {
    room.tags.retain(|tag| tag.name != FAVOURITE_TAG);
}

fn restore_tags(rooms: &mut [RoomDto], previous: &HashMap<RoomKey, Vec<RoomTag>>) {
    for room in rooms {
        if let Some(tags) = previous.get(&RoomKey::from(&*room)) {
            room.tags.clone_from(tags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::rooms::sort_rooms_by_pin;
    use crate::app::RoomSort;

    fn room(account_id: Uuid, room_id: &str, ts: i64) -> RoomDto {
        RoomDto {
            account_id,
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: room_id.to_owned(),
            name: None,
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            room_type: None,
            last_activity_ts: ts,
            last_event_id: None,
            tags: Vec::new(),
            is_direct: false,
        }
    }

    fn favourite(mut room: RoomDto, order: Option<f64>) -> RoomDto {
        room.tags.push(RoomTag {
            name: FAVOURITE_TAG.to_owned(),
            order,
        });
        room
    }

    #[test]
    fn pin_to_top_first_favourite_uses_mid_range_order() {
        let acct = Uuid::nil();
        let rooms = vec![room(acct, "!a:srv", 1)];
        let target = RoomKey {
            account_id: acct,
            room_id: "!a:srv".to_owned(),
        };
        let assignments = pin_to_top_assignments(&rooms, &target);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].order, 0.5);
    }

    #[test]
    fn pin_to_top_splits_the_current_minimum() {
        let acct = Uuid::nil();
        let rooms = vec![
            favourite(room(acct, "!a:srv", 1), Some(0.4)),
            room(acct, "!b:srv", 2),
        ];
        let target = RoomKey {
            account_id: acct,
            room_id: "!b:srv".to_owned(),
        };
        let assignments = pin_to_top_assignments(&rooms, &target);
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].order, 0.2);
    }

    #[test]
    fn pin_to_top_rebalances_when_minimum_is_zero() {
        let acct = Uuid::nil();
        let rooms = vec![
            favourite(room(acct, "!a:srv", 1), Some(0.0)),
            favourite(room(acct, "!b:srv", 2), Some(0.5)),
            room(acct, "!c:srv", 3),
        ];
        let target = RoomKey {
            account_id: acct,
            room_id: "!c:srv".to_owned(),
        };
        let assignments = pin_to_top_assignments(&rooms, &target);
        assert_eq!(assignments[0].key.room_id, "!c:srv");
        assert_eq!(assignments[0].order, 0.0);
        assert_eq!(assignments.len(), 3);
        assert!(assignments[1].order > 0.0);
        assert!(assignments[2].order > assignments[1].order);
    }

    #[test]
    fn sort_favourite_order_then_unpinned_by_activity() {
        let acct = Uuid::nil();
        let mut rooms = vec![
            favourite(room(acct, "!a:srv", 1), Some(0.8)),
            room(acct, "!b:srv", 9),
            favourite(room(acct, "!c:srv", 2), Some(0.1)),
            room(acct, "!d:srv", 5),
        ];
        sort_rooms_by_pin(&mut rooms, &[], RoomSort::RecentActivity);
        let order: Vec<&str> = rooms.iter().map(|r| r.room_id.as_str()).collect();
        assert_eq!(order, vec!["!c:srv", "!a:srv", "!b:srv", "!d:srv"]);
    }

    #[test]
    fn migration_assignments_are_index_over_n_per_account() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let pinned = vec![
            RoomKey {
                account_id: a,
                room_id: "!top:srv".to_owned(),
            },
            RoomKey {
                account_id: a,
                room_id: "!next:srv".to_owned(),
            },
            RoomKey {
                account_id: b,
                room_id: "!only:srv".to_owned(),
            },
        ];
        let assignments = migration_assignments(&pinned);
        let a_orders: Vec<f64> = assignments
            .iter()
            .filter(|item| item.key.account_id == a)
            .map(|item| item.order)
            .collect();
        assert_eq!(a_orders, vec![0.0, 0.5]);
        let b_orders: Vec<f64> = assignments
            .iter()
            .filter(|item| item.key.account_id == b)
            .map(|item| item.order)
            .collect();
        assert_eq!(b_orders, vec![0.0]);
    }
}
