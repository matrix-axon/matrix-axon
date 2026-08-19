//! Cross-device read markers over the M12 device-state API (ADR 0048).
//!
//! Each room's last-read position is mirrored to Axon's per-device state under
//! the `read_markers` namespace (key = room id, value
//! `{"event_id": …, "origin_ts": …}`, scoped by `account_id`), so reading a
//! room on one device clears its unread badge on the user's other devices, and
//! unread state survives a restart instead of resetting with the session.
//!
//! The wire machinery is the drafts machinery ([`super::drafts`]): a debounced
//! PUT per settled change, hydration from the merged view at startup and on
//! every WS (re)connect, live `device_state.changed` frames from sibling
//! devices, and echo suppression by device id.
//!
//! **Monotonicity is a client-side convention.** The server's last-write-wins
//! is arrival-order, so a device that was offline can legitimately win with a
//! marker *older* than the current one. Reading never moves backward, so this
//! client refuses backward movement everywhere: it neither applies an older
//! incoming marker nor arms a PUT for one. Tombstones (a `null` marker) have
//! no meaning for read state and are ignored.
//!
//! # Two values, two orders (ADR 0089)
//!
//! Reading a room settles **two** independent positions, and this module tracks
//! them separately. Do not merge them back into one:
//!
//! - [`ReadMarker`] is the cross-device device-state marker above, and the key
//!   unread detection compares against (`origin_ts > marker_ts`). It is a
//!   *display-order* artifact — where to draw the "new messages" line — ordered
//!   on `origin_ts`.
//! - [`ReceiptTarget`] is what the Matrix read receipt names. A receipt is
//!   interpreted by the homeserver in **arrival** order, so it is ordered on
//!   `EventDto::arrival_order` and names the greatest one among the events this
//!   client actually displayed.
//!
//! The two orders agree until a homeserver delivers an event stamped earlier
//! than events already held — routinely, for a bridge that creates a portal,
//! emits its own state, and *then* backfills the pre-existing conversation with
//! its real, older timestamps. The room's only message is then oldest by
//! `origin_ts` and newest by arrival order. Receipting the display-newest event
//! there names a portal state event that does not cover the message, so the room
//! shows unread on every load; and because the marker is forward-only on
//! `origin_ts`, it is already *ahead* of that message and can never move again —
//! which is why the receipt must advance independently of the marker, not as a
//! passenger on it (see [`App::note_room_read`]).
//!
//! The receipt target is **session-local** and deliberately not hydrated: it is
//! not device state. An empty map after a restart is what lets a room already
//! broken by this bug repair itself on its next open.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;
use uuid::Uuid;

use super::drafts::DRAFT_DEBOUNCE;
use super::{App, LiveFrameAction, RoomKey};

/// The device-state namespace read markers live under.
pub(crate) const READ_MARKERS_NAMESPACE: &str = "read_markers";

/// A room's last-read position: the newest event the user had on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadMarker {
    pub(crate) event_id: String,
    /// `origin_server_ts` of that event, milliseconds — the monotonic ordering.
    pub(crate) origin_ts: i64,
}

/// A room's read-receipt target: the greatest-`arrival_order` event this client
/// has displayed there (ADR 0089).
///
/// Separate from [`ReadMarker`] because the two order on different keys and
/// genuinely disagree — see the module docs. Session-local, never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReceiptTarget {
    pub(crate) event_id: String,
    /// `EventDto::arrival_order` of that event — the monotonic ordering for
    /// receipts, which is *not* `origin_ts` order.
    pub(crate) arrival_order: i64,
}

/// The two read positions a loaded page settles, from one pass over one
/// candidate set.
///
/// Both or neither: each is derived from the same non-empty set of displayed
/// events, so a page with nothing on screen yields `None` rather than a marker
/// without a receipt. Returning them together is what makes the shared
/// candidate set structural instead of a convention two call sites have to
/// remember (#167).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadTargets {
    /// Display-order: the page's display-*last* event, newest by `origin_ts`
    /// because a page is installed in ascending order.
    pub(crate) marker: ReadMarker,
    /// Arrival-order: the greatest `arrival_order` among the same events.
    pub(crate) receipt: ReceiptTarget,
}

/// The [`ReadTargets`] for `events`, or `None` when the page displays nothing.
///
/// The filter matters: ADR 0089's rule is "among the events it has actually
/// displayed", so this has to match what the main timeline renders —
/// [`App::selected_events`]'s pair of `should_show_event` **and**
/// `thread_visible`, not either one alone. It keeps the bug case honest: the
/// `uk.half-shot.bridge` state event that started all this is hidden at the
/// default settings, so it must not be a legal target even when it is
/// arrival-max.
///
/// `thread_visible` is checked with `thread_panel: None`, because this answers
/// "what does the *main* timeline show" and the only caller (room open) clears
/// the panel moments later. Without it a thread reply could be receipted while
/// hidden behind its root's badge, on two paths — a room's first-ever load,
/// where `collect_unseen_thread_promotions` has no marker to measure against
/// and promotes nothing at all; and any load where the reply is *backfilled*,
/// since promotion requires `origin_ts > marker_ts` and a backfilled reply is
/// old by `origin_ts` while being new by `arrival_order`. That second path is
/// this ADR's own scenario, which is what makes the omission more than
/// theoretical. Caught in review on #165.
///
/// The live path in [`super::timeline`] needs no equivalent: it promotes a
/// thread reply into `promoted_thread_events` before it reaches the read gate
/// (or the panel is open on that root, which displays it), so anything that
/// gets there is visible by construction.
pub(crate) fn read_targets_for(
    events: &[crate::api::EventDto],
    display: &crate::config::DisplayOptions,
    promoted: &std::collections::HashSet<String>,
) -> Option<ReadTargets> {
    // One pass yields both positions. `>=` keeps the *last* of equal
    // `arrival_order` values, matching the `max_by_key` this replaced.
    let mut found: Option<(&crate::api::EventDto, &crate::api::EventDto)> = None;
    for event in displayed_events(events, display, promoted) {
        found = Some(match found {
            None => (event, event),
            Some((_, arrival_max)) => (
                event,
                if event.arrival_order >= arrival_max.arrival_order {
                    event
                } else {
                    arrival_max
                },
            ),
        });
    }
    found.map(|(display_last, arrival_max)| ReadTargets {
        marker: ReadMarker {
            event_id: display_last.event_id.clone(),
            origin_ts: display_last.origin_ts,
        },
        receipt: ReceiptTarget {
            event_id: arrival_max.event_id.clone(),
            arrival_order: arrival_max.arrival_order,
        },
    })
}

/// The events in `events` the main timeline actually displays, in page order:
/// the pair `should_show_event` **and** `thread_visible`, matching
/// [`App::selected_events`].
fn displayed_events<'a>(
    events: &'a [crate::api::EventDto],
    display: &'a crate::config::DisplayOptions,
    promoted: &'a std::collections::HashSet<String>,
) -> impl Iterator<Item = &'a crate::api::EventDto> {
    events
        .iter()
        .filter(move |event| super::timeline::should_show_event(event, display))
        .filter(move |event| super::relations::thread_visible(event, None, promoted))
}

/// A marker advance waiting out its debounce window before being PUT.
pub(crate) struct PendingMarkerPut {
    pub(crate) room: RoomKey,
    pub(crate) marker: ReadMarker,
    pub(crate) due: Instant,
}

/// The marker's wire value.
fn marker_value(marker: &ReadMarker) -> Value {
    serde_json::json!({ "event_id": marker.event_id, "origin_ts": marker.origin_ts })
}

/// Parse a marker from its wire value; `None` for shapes this client doesn't
/// recognize (left alone rather than misread).
fn marker_from_value(value: &Value) -> Option<ReadMarker> {
    Some(ReadMarker {
        event_id: value.get("event_id")?.as_str()?.to_owned(),
        origin_ts: value.get("origin_ts")?.as_i64()?,
    })
}

impl App {
    /// Mark as promoted every thread member in `events` newer than the room's
    /// read marker, returning the (deduplicated) thread roots so the caller
    /// can fetch their context once the page is installed.
    ///
    /// This is what keeps an unread badge honest for an **old thread**: a new
    /// reply hides behind its root's badge (`thread_visible`), and for a
    /// thread whose root has scrolled out of the loaded window there is no
    /// badge on screen at all — the room says unread but shows nothing new.
    /// The reply that caused the badge is by definition newer than the
    /// marker, so promoting everything past the marker surfaces it inline,
    /// exactly like the existing live-arrival promotion. Must be called
    /// *before* [`App::note_room_read`] advances the marker; a room with no
    /// marker yet keeps today's behavior (nothing to measure "unseen"
    /// against).
    ///
    /// The same past-the-marker replies also feed the unread-thread attention
    /// markers (ADR 0049): live observation alone misses replies that arrived
    /// while this client was down, but the read marker knows exactly what was
    /// never seen, so those replies count toward their root's badge and the
    /// `/unreadthreads` picker too (own messages excepted, matching the live
    /// path). Thread-unread state still clears only when the thread panel
    /// opens, not on room entry.
    pub(crate) fn collect_unseen_thread_promotions(
        &mut self,
        room: &RoomKey,
        events: &[crate::api::EventDto],
    ) -> Vec<(Uuid, String)> {
        let Some(marker_ts) = self.read_markers.get(room).map(|m| m.origin_ts) else {
            return Vec::new();
        };
        let mut roots: Vec<(Uuid, String)> = Vec::new();
        for event in events.iter().filter(|e| e.origin_ts > marker_ts) {
            let Some(root) = event.thread_relation().map(str::to_owned) else {
                continue;
            };
            self.promoted_thread_events.insert(event.event_id.clone());
            if self.thread_event_counts_as_unread(event, None) {
                self.mark_thread_unread_from_event(room, &root, event);
            }
            let entry = (event.account_id, root);
            if !roots.contains(&entry) {
                roots.push(entry);
            }
        }
        roots
    }

    /// The user has the room on screen read up to `marker` in display order and
    /// up to `receipt` in arrival order: advance whichever of
    /// the two positions actually moved, and arm the debounced PUT if either
    /// did. Backward or repeated positions are no-ops (monotonic, per key).
    /// Called from the timeline-load and live-event paths that already clear the
    /// room's unread badge.
    ///
    /// The two advances are computed **independently**, and the arm is `||` over
    /// both, because that is the entire bug (ADR 0089). A room broken by it has
    /// a marker sitting at a portal state event's `origin_ts`, permanently ahead
    /// of the backfilled message it should have receipted; gating the receipt on
    /// the marker advancing would mean no such room is ever repaired — only
    /// rooms bridged after this ships would benefit.
    pub(crate) fn note_room_read(
        &mut self,
        room: RoomKey,
        marker: ReadMarker,
        receipt: Option<ReceiptTarget>,
    ) {
        let fallback = marker.clone();
        let marker_advanced = self.apply_marker(room.clone(), marker);
        let receipt_advanced =
            receipt.is_some_and(|target| self.apply_receipt_target(room.clone(), target));
        if !marker_advanced && !receipt_advanced {
            return;
        }
        // One pending slot: a still-armed advance for a *different* room (the
        // user read it and switched away inside the debounce window) is sent
        // now rather than silently replaced.
        if let Some(pending) = self.pending_marker_put.take() {
            if pending.room != room {
                self.spawn_current_read_put(pending.room, pending.marker);
            }
        }
        // The armed marker is only a fallback for `spawn_current_read_put`; the
        // map is what actually gets sent, for both halves.
        //
        // `apply_marker` above leaves an entry for this room on both of its
        // paths — it either found one already at or ahead of ours, or inserted
        // — so the fallback is unreachable today. It stays because a running
        // TUI should degrade to the value it was just handed rather than panic
        // if that ever stops holding, and the assertion is what keeps such a
        // gap from being silently masked (review note on #165).
        debug_assert!(
            self.read_markers.contains_key(&room),
            "apply_marker must leave a marker for the room it was given"
        );
        let marker = self.read_markers.get(&room).cloned().unwrap_or(fallback);
        self.pending_marker_put = Some(PendingMarkerPut {
            room,
            marker,
            due: Instant::now() + DRAFT_DEBOUNCE,
        });
    }

    /// Advance a room's receipt target, monotonically on `arrival_order`.
    /// Returns whether it moved.
    fn apply_receipt_target(&mut self, room: RoomKey, target: ReceiptTarget) -> bool {
        if self
            .receipt_targets
            .get(&room)
            .is_some_and(|current| current.arrival_order >= target.arrival_order)
        {
            return false;
        }
        self.receipt_targets.insert(room, target);
        true
    }

    /// Called on the main-loop tick: flush the pending marker once its
    /// debounce window has passed.
    pub(crate) fn flush_due_marker_put(&mut self, now: Instant) {
        if self.pending_marker_put.as_ref().is_none_or(|p| now < p.due) {
            return;
        }
        let Some(pending) = self.pending_marker_put.take() else {
            return;
        };
        self.spawn_current_read_put(pending.room, pending.marker);
    }

    /// The pair to send for a room: its *current* marker and receipt target, not
    /// the values captured when the slot was armed. A sibling-device frame
    /// (`handle_read_marker_frame`) can advance `self.read_markers` during the
    /// debounce window, and sending the stale armed value would then overwrite
    /// the server with an older position. Monotonicity guarantees the current
    /// value is never behind the armed one, which is used only as a fallback if
    /// the entry has somehow gone. The receipt target has no armed fallback: it
    /// is only ever set by `note_room_read`, which arms the slot in the same
    /// breath, and `None` simply means this room has no displayed event to name.
    fn current_read_put(
        &self,
        room: &RoomKey,
        armed: ReadMarker,
    ) -> (ReadMarker, Option<ReceiptTarget>) {
        (
            self.read_markers.get(room).cloned().unwrap_or(armed),
            self.receipt_targets.get(room).cloned(),
        )
    }

    /// PUT a room's current marker and send its current receipt.
    fn spawn_current_read_put(&self, room: RoomKey, armed: ReadMarker) {
        let (marker, receipt) = self.current_read_put(&room, armed);
        self.spawn_read_put(room, marker, receipt);
    }

    /// PUT one room's marker in the background, and send its read receipt.
    /// Failures surface through the same channel as draft PUTs; not wired (unit
    /// tests) means local-only, like the other background channels.
    ///
    /// The device-state PUT fires whenever the slot was armed, even when only
    /// the receipt moved. Suppressing it would need a "did the marker advance?"
    /// flag OR-accumulated across every re-arm inside the sliding debounce
    /// window, and getting that wrong stops publishing cross-device markers —
    /// a failure invisible until another device is wrong. What it would save is
    /// one idempotent PUT in a rare path, whose only effect is a
    /// `device_state.changed` frame that siblings' own `apply_marker` refuses.
    fn spawn_read_put(&self, room: RoomKey, marker: ReadMarker, receipt: Option<ReceiptTarget>) {
        let Some(tx) = self.drafts_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        let device_id = self.device_id;
        // Second, fire-and-forget action alongside the internal device-state PUT
        // (ADR 0067): tell the homeserver too, so third-party Matrix clients see
        // the room as read. Same debounced choke point, best-effort — a failed
        // receipt is never surfaced (the local read UX must not depend on it,
        // and the TUI has no in-session log surface to write to).
        //
        // It names the receipt *target*, not the marker: a receipt is
        // interpreted in arrival order and the marker is display order (ADR
        // 0089, module docs).
        if let Some(target) = receipt {
            let client = client.clone();
            let room = room.clone();
            tokio::spawn(async move {
                let _ = client
                    .send_read_receipt(room.account_id, &room.room_id, &target.event_id)
                    .await;
            });
        }
        tokio::spawn(async move {
            let entries: HashMap<String, Option<Value>> =
                HashMap::from([(room.room_id.clone(), Some(marker_value(&marker)))]);
            if let Err(err) = client
                .put_device_state(device_id, room.account_id, READ_MARKERS_NAMESPACE, &entries)
                .await
            {
                let _ = tx.send(super::DraftOutcome::PutFailed(format!(
                    "read marker: {err}"
                )));
            }
        });
    }

    /// Hydrate read markers from the server's merged view, one GET per active
    /// account, then reconcile the unread badges. Called at startup and on
    /// every WS (re)connect (the lossy bus may have dropped frames). A failed
    /// read leaves that account's local markers as they are.
    pub(crate) async fn refresh_read_markers(&mut self) {
        let device_id = self.device_id;
        let account_ids: Vec<Uuid> = self
            .accounts
            .accounts
            .iter()
            .map(|a| a.account_id)
            .collect();
        // Fetch every account's merged view concurrently: this runs inline in
        // the event loop, so a sequential GET-per-account would freeze the UI
        // for account_count × RTT at startup or after a reconnect (matches
        // refresh_drafts).
        let reads = account_ids.into_iter().map(|account_id| {
            let client = self.client.clone();
            async move {
                (
                    account_id,
                    client
                        .get_device_state(device_id, account_id, READ_MARKERS_NAMESPACE)
                        .await,
                )
            }
        });
        let results = futures_util::future::join_all(reads).await;
        for (account_id, result) in results {
            let Ok(state) = result else {
                continue;
            };
            for (room_id, entry) in state.entries {
                let Some(marker) = marker_from_value(&entry.value) else {
                    continue;
                };
                self.apply_marker(
                    RoomKey {
                        account_id,
                        room_id,
                    },
                    marker,
                );
            }
        }
        self.reconcile_unread_with_markers();
    }

    /// Apply one live `read_markers` entry map from a sibling device.
    pub(crate) fn handle_read_marker_frame(
        &mut self,
        account_id: Uuid,
        entries: HashMap<String, Value>,
    ) -> LiveFrameAction {
        for (room_id, value) in entries {
            // A tombstone has no meaning for read state; skip it.
            let Some(marker) = marker_from_value(&value) else {
                continue;
            };
            let key = RoomKey {
                account_id,
                room_id,
            };
            if self.apply_marker(key.clone(), marker) {
                self.clear_unread_if_read(&key);
            }
        }
        LiveFrameAction::None
    }

    /// Merge one marker into the local map, monotonically. Returns whether it
    /// advanced.
    fn apply_marker(&mut self, room: RoomKey, marker: ReadMarker) -> bool {
        if self
            .read_markers
            .get(&room)
            .is_some_and(|current| current.origin_ts >= marker.origin_ts)
        {
            return false;
        }
        self.read_markers.insert(room, marker);
        true
    }

    /// Reconcile every room's unread badge with its marker: a room whose
    /// latest known activity is newer than its marker shows as unread (this is
    /// what survives a restart), one read to (or past) its latest activity
    /// clears. Rooms with no marker are left alone — a first run must not
    /// light up every room.
    pub(crate) fn reconcile_unread_with_markers(&mut self) {
        let rooms: Vec<(RoomKey, i64)> = self
            .rooms
            .rooms
            .iter()
            .map(|room| (RoomKey::from(room), room.last_activity_ts))
            .collect();
        for (key, listed_ts) in rooms {
            let Some(marker) = self.read_markers.get(&key) else {
                continue;
            };
            let marker_ts = marker.origin_ts;
            // Reuse the room-list ts collected above instead of re-scanning
            // self.rooms.rooms per room (which made this O(n²) in room count).
            if listed_ts.max(self.loaded_activity_ts(&key)) > marker_ts {
                self.rooms.unread.entry(key).or_insert(1);
            } else {
                self.rooms.unread.remove(&key);
            }
        }
    }

    /// Drop a room's unread badge when its marker has caught up with its
    /// latest known activity — the user read it on another device.
    fn clear_unread_if_read(&mut self, room: &RoomKey) {
        let Some(marker) = self.read_markers.get(room) else {
            return;
        };
        if marker.origin_ts >= self.latest_known_activity_ts(room) {
            self.rooms.unread.remove(room);
        }
    }

    /// The newest activity this client knows for a room: the room list's
    /// `last_activity_ts` or the newest loaded/live event, whichever is later
    /// (live events don't update the room-list timestamp between refreshes).
    fn latest_known_activity_ts(&self, room: &RoomKey) -> i64 {
        let listed = self
            .rooms
            .rooms
            .iter()
            .find(|r| RoomKey::from(*r) == *room)
            .map(|r| r.last_activity_ts)
            .unwrap_or(0);
        listed.max(self.loaded_activity_ts(room))
    }

    /// The newest `origin_ts` among the room's loaded/live events, or 0.
    fn loaded_activity_ts(&self, room: &RoomKey) -> i64 {
        self.messages
            .events
            .get(room)
            .and_then(|events| events.iter().map(|e| e.origin_ts).max())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AxonClient, EventDto, RoomDto};
    use crate::config::TuiConfig;
    use ratatui_image::picker::Picker;

    fn test_room(room_id: &str, last_activity_ts: i64) -> RoomDto {
        RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: room_id.to_owned(),
            name: Some("Room".to_owned()),
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            last_activity_ts,
            last_event_id: None,
        }
    }

    fn app_with(rooms: Vec<RoomDto>) -> App {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.rooms.rooms = rooms;
        app.seed_own_senders_from_rooms();
        app.set_device_id(Uuid::new_v4());
        app
    }

    fn key(room_id: &str) -> RoomKey {
        RoomKey {
            account_id: Uuid::nil(),
            room_id: room_id.to_owned(),
        }
    }

    fn marker(event_id: &str, origin_ts: i64) -> ReadMarker {
        ReadMarker {
            event_id: event_id.to_owned(),
            origin_ts,
        }
    }

    /// A receipt target that moves in step with the marker — the ordinary case,
    /// where display and arrival order agree.
    fn in_step(event_id: &str, ts: i64) -> Option<ReceiptTarget> {
        Some(ReceiptTarget {
            event_id: event_id.to_owned(),
            arrival_order: ts,
        })
    }

    #[test]
    fn note_room_read_is_monotonic() {
        let mut app = app_with(vec![test_room("!r:x", 0)]);
        let k = key("!r:x");

        app.note_room_read(k.clone(), marker("$b", 200), in_step("$b", 200));
        assert_eq!(app.read_markers.get(&k).unwrap().origin_ts, 200);
        assert!(app.pending_marker_put.is_some());

        // Backward (or equal) never regresses the marker or re-arms a PUT.
        // Both keys move backward together here: with neither advancing, the
        // slot must stay empty.
        app.pending_marker_put = None;
        app.note_room_read(k.clone(), marker("$a", 100), in_step("$a", 100));
        app.note_room_read(k.clone(), marker("$b", 200), in_step("$b", 200));
        assert_eq!(app.read_markers.get(&k).unwrap().event_id, "$b");
        assert!(app.pending_marker_put.is_none());

        // Forward advances.
        app.note_room_read(k.clone(), marker("$c", 300), in_step("$c", 300));
        assert_eq!(app.read_markers.get(&k).unwrap().origin_ts, 300);
        assert!(app.pending_marker_put.is_some());
    }

    /// ADR 0089's core case, and the one that repairs already-broken rooms.
    ///
    /// A bridge portal's marker sits at its `uk.half-shot.bridge` state event,
    /// which is stamped *after* the backfilled message it should have receipted.
    /// The marker is forward-only on `origin_ts`, so it can never move again —
    /// if the receipt rode on the marker advancing, this room would stay unread
    /// forever and the fix would only ever help rooms bridged after it shipped.
    #[test]
    fn receipt_advances_when_the_marker_cannot() {
        let mut app = app_with(vec![test_room("!r:x", 0)]);
        let k = key("!r:x");
        // As hydration from device state would leave it.
        app.read_markers.insert(
            k.clone(),
            ReadMarker {
                event_id: "$bridge".to_owned(),
                origin_ts: 1_785_928_309_453,
            },
        );

        // The backfilled message: older by origin_ts, newer by arrival order.
        app.note_room_read(
            k.clone(),
            marker("$bridge", 1_785_928_309_453),
            Some(ReceiptTarget {
                event_id: "$message".to_owned(),
                arrival_order: 1_871_426,
            }),
        );

        // The marker refused, correctly, and did not move.
        assert_eq!(app.read_markers.get(&k).unwrap().event_id, "$bridge");
        // The receipt advanced anyway, and the PUT is armed to send it.
        assert_eq!(
            app.receipt_targets.get(&k),
            Some(&ReceiptTarget {
                event_id: "$message".to_owned(),
                arrival_order: 1_871_426,
            })
        );
        assert!(app.pending_marker_put.is_some());
    }

    #[test]
    fn receipt_target_is_monotonic_on_arrival_order() {
        let mut app = app_with(vec![test_room("!r:x", 0)]);
        let k = key("!r:x");
        app.note_room_read(
            k.clone(),
            marker("$a", 100),
            Some(ReceiptTarget {
                event_id: "$a".to_owned(),
                arrival_order: 900,
            }),
        );
        app.pending_marker_put = None;

        // Newer in display order, *older* in arrival order: the marker would
        // take this, the receipt must not.
        app.note_room_read(
            k.clone(),
            marker("$b", 200),
            Some(ReceiptTarget {
                event_id: "$b".to_owned(),
                arrival_order: 500,
            }),
        );
        assert_eq!(app.read_markers.get(&k).unwrap().event_id, "$b");
        assert_eq!(app.receipt_targets.get(&k).unwrap().event_id, "$a");
        // The marker moved, so the slot is armed regardless.
        assert!(app.pending_marker_put.is_some());

        // Neither key moves → no arm at all.
        app.pending_marker_put = None;
        app.note_room_read(
            k.clone(),
            marker("$b", 200),
            Some(ReceiptTarget {
                event_id: "$b".to_owned(),
                arrival_order: 500,
            }),
        );
        assert!(app.pending_marker_put.is_none());
    }

    /// Guards the `||` from the other side: a marker-only advance still arms,
    /// and leaves the receipt target alone.
    #[test]
    fn marker_advance_alone_still_arms() {
        let mut app = app_with(vec![test_room("!r:x", 0)]);
        let k = key("!r:x");

        app.note_room_read(k.clone(), marker("$a", 100), None);
        assert_eq!(app.read_markers.get(&k).unwrap().event_id, "$a");
        assert!(!app.receipt_targets.contains_key(&k));
        assert!(app.pending_marker_put.is_some());

        // A receipt at an arrival order already held is not an advance either.
        app.note_room_read(
            k.clone(),
            marker("$b", 200),
            Some(ReceiptTarget {
                event_id: "$b".to_owned(),
                arrival_order: 700,
            }),
        );
        app.pending_marker_put = None;
        app.note_room_read(
            k.clone(),
            marker("$b", 200),
            Some(ReceiptTarget {
                event_id: "$b-again".to_owned(),
                arrival_order: 700,
            }),
        );
        assert_eq!(app.receipt_targets.get(&k).unwrap().event_id, "$b");
        assert!(app.pending_marker_put.is_none());
    }

    /// The "current, not armed" rule, which the spawn path itself cannot test
    /// (no `drafts_tx` is wired, so it is inert). A sibling-device frame can
    /// advance either map inside the debounce window; both halves must be read
    /// from the map at flush time.
    #[test]
    fn flush_sends_the_current_values_not_the_armed_ones() {
        let mut app = app_with(vec![test_room("!r:x", 0)]);
        let k = key("!r:x");
        app.note_room_read(k.clone(), marker("$a", 100), in_step("$a", 100));
        let armed = app.pending_marker_put.as_ref().unwrap().marker.clone();
        assert_eq!(armed.event_id, "$a");

        // Both positions move on after the slot was armed.
        app.apply_marker(
            k.clone(),
            ReadMarker {
                event_id: "$c".to_owned(),
                origin_ts: 300,
            },
        );
        app.apply_receipt_target(
            k.clone(),
            ReceiptTarget {
                event_id: "$c".to_owned(),
                arrival_order: 300,
            },
        );

        let (marker, receipt) = app.current_read_put(&k, armed);
        assert_eq!(marker.event_id, "$c");
        assert_eq!(receipt.unwrap().event_id, "$c");
    }

    #[test]
    fn incoming_frames_apply_monotonically_and_clear_unread() {
        let mut app = app_with(vec![test_room("!r:x", 250)]);
        let k = key("!r:x");
        app.rooms.unread.insert(k.clone(), 3);

        // A marker older than the room's latest activity: applied, badge stays.
        let entries = HashMap::from([(
            "!r:x".to_owned(),
            marker_value(&ReadMarker {
                event_id: "$old".to_owned(),
                origin_ts: 100,
            }),
        )]);
        app.handle_read_marker_frame(Uuid::nil(), entries);
        assert_eq!(app.read_markers.get(&k).unwrap().origin_ts, 100);
        assert_eq!(app.rooms.unread.get(&k), Some(&3));

        // A marker at (or past) the latest activity clears the badge.
        let entries = HashMap::from([(
            "!r:x".to_owned(),
            marker_value(&ReadMarker {
                event_id: "$new".to_owned(),
                origin_ts: 250,
            }),
        )]);
        app.handle_read_marker_frame(Uuid::nil(), entries);
        assert!(!app.rooms.unread.contains_key(&k));

        // An older marker arriving later (offline device replay) is refused.
        let entries = HashMap::from([(
            "!r:x".to_owned(),
            marker_value(&ReadMarker {
                event_id: "$stale".to_owned(),
                origin_ts: 50,
            }),
        )]);
        app.handle_read_marker_frame(Uuid::nil(), entries);
        assert_eq!(app.read_markers.get(&k).unwrap().event_id, "$new");

        // Tombstones and unknown shapes are ignored.
        let entries = HashMap::from([("!r:x".to_owned(), Value::Null)]);
        app.handle_read_marker_frame(Uuid::nil(), entries);
        assert_eq!(app.read_markers.get(&k).unwrap().event_id, "$new");
    }

    #[test]
    fn reconcile_seeds_unread_only_for_marked_rooms_with_newer_activity() {
        let mut app = app_with(vec![
            test_room("!behind:x", 500),    // marker older than activity → unread
            test_room("!caught-up:x", 300), // marker at activity → read
            test_room("!unmarked:x", 900),  // no marker → left alone
        ]);
        app.read_markers.insert(
            key("!behind:x"),
            ReadMarker {
                event_id: "$m1".to_owned(),
                origin_ts: 400,
            },
        );
        app.read_markers.insert(
            key("!caught-up:x"),
            ReadMarker {
                event_id: "$m2".to_owned(),
                origin_ts: 300,
            },
        );
        // A stale badge on the caught-up room (e.g. counted while the socket
        // was down, then read on another device) is dropped by reconcile.
        app.rooms.unread.insert(key("!caught-up:x"), 2);

        app.reconcile_unread_with_markers();

        assert_eq!(app.rooms.unread.get(&key("!behind:x")), Some(&1));
        assert!(!app.rooms.unread.contains_key(&key("!caught-up:x")));
        assert!(!app.rooms.unread.contains_key(&key("!unmarked:x")));
    }

    #[test]
    fn reconcile_does_not_shrink_a_live_count() {
        let mut app = app_with(vec![test_room("!r:x", 500)]);
        let k = key("!r:x");
        app.read_markers.insert(
            k.clone(),
            ReadMarker {
                event_id: "$m".to_owned(),
                origin_ts: 400,
            },
        );
        // Three live events already counted this session; reconcile must not
        // collapse the count to 1.
        app.rooms.unread.insert(k.clone(), 3);
        app.reconcile_unread_with_markers();
        assert_eq!(app.rooms.unread.get(&k), Some(&3));
    }

    #[test]
    fn switching_rooms_mid_debounce_flushes_the_previous_marker() {
        let mut app = app_with(vec![test_room("!a:x", 0), test_room("!b:x", 0)]);
        app.note_room_read(key("!a:x"), marker("$a", 100), in_step("$a", 100));
        assert_eq!(app.pending_marker_put.as_ref().unwrap().room, key("!a:x"));

        // Reading a different room replaces the slot; the previous marker is
        // flushed (spawn is a no-op here with no channel wired) — the local
        // map keeps both.
        app.note_room_read(key("!b:x"), marker("$b", 200), in_step("$b", 200));
        assert_eq!(app.pending_marker_put.as_ref().unwrap().room, key("!b:x"));
        assert_eq!(app.read_markers.get(&key("!a:x")).unwrap().origin_ts, 100);
        assert_eq!(app.read_markers.get(&key("!b:x")).unwrap().origin_ts, 200);
        // Receipt targets are per-room too: reading B must not disturb A's.
        assert_eq!(
            app.receipt_targets.get(&key("!a:x")).unwrap().arrival_order,
            100
        );
        assert_eq!(
            app.receipt_targets.get(&key("!b:x")).unwrap().arrival_order,
            200
        );
    }

    fn timeline_event_from(
        sender: &str,
        event_id: &str,
        origin_ts: i64,
        thread_root: Option<&str>,
    ) -> EventDto {
        let mut event = timeline_event(event_id, origin_ts, thread_root);
        event.sender = sender.to_owned();
        event
    }

    /// An event from the account's own user (`test_room` sets
    /// `account_user_id` to this sender).
    fn timeline_event(event_id: &str, origin_ts: i64, thread_root: Option<&str>) -> EventDto {
        EventDto {
            account_id: Uuid::nil(),
            event_id: event_id.to_owned(),
            room_id: "!r:x".to_owned(),
            sender: "@alice:example.com".to_owned(),
            state_key: None,
            arrival_order: origin_ts,
            origin_ts,
            event_type: "m.room.message".to_owned(),
            content: Some(serde_json::json!({ "msgtype": "m.text", "body": "hi" })),
            body: Some("hi".to_owned()),
            relates_to: thread_root
                .map(|root| serde_json::json!({ "rel_type": "m.thread", "event_id": root })),
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        }
    }

    fn display_with_state_events(show_state_events: bool) -> crate::config::DisplayOptions {
        let mut display = TuiConfig::test_default().display;
        display.show_state_events = show_state_events;
        display
    }

    /// An event with display and arrival order deliberately out of step.
    fn arrival_event(event_id: &str, origin_ts: i64, arrival_order: i64) -> EventDto {
        let mut event = timeline_event(event_id, origin_ts, None);
        event.arrival_order = arrival_order;
        event
    }

    fn state_event(event_id: &str, origin_ts: i64, arrival_order: i64, ty: &str) -> EventDto {
        let mut event = arrival_event(event_id, origin_ts, arrival_order);
        event.event_type = ty.to_owned();
        event.state_key = Some(String::new());
        event.body = None;
        event
    }

    /// The LinkedIn portal from ADR 0089, with its real numbers. The bridge
    /// creates the room, emits its own state, then backfills the conversation
    /// with its real (older) timestamps — so the only message is oldest by
    /// `origin_ts` and newest by arrival order.
    fn portal_page() -> Vec<EventDto> {
        vec![
            // Ascending display order, which is how a loaded page is held.
            arrival_event("$message", 1_785_928_304_987, 1_871_426),
            state_event("$create", 1_785_928_306_622, 1_871_406, "m.room.create"),
            state_event(
                "$bridge",
                1_785_928_309_453,
                1_871_424,
                "uk.half-shot.bridge",
            ),
        ]
    }

    /// No thread reply has been promoted into the main timeline. The common
    /// case, and the one that matters: promotion is what makes a reply visible
    /// outside its thread panel.
    fn nothing_promoted() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn receipt_target_picks_arrival_max_not_display_last() {
        let display = display_with_state_events(true);
        let page = portal_page();

        // Display-last is the bridge state event — what the old code sent, and
        // what does not cover the message in stream order.
        assert_eq!(page.last().unwrap().event_id, "$bridge");
        assert_eq!(
            read_targets_for(&page, &display, &nothing_promoted())
                .unwrap()
                .receipt
                .event_id,
            "$message"
        );
    }

    #[test]
    fn receipt_target_skips_hidden_events() {
        // At the default settings the portal's state events do not render, so
        // they are not candidates — which is also the right answer here.
        let hidden = display_with_state_events(false);
        assert_eq!(
            read_targets_for(&portal_page(), &hidden, &nothing_promoted())
                .unwrap()
                .receipt
                .event_id,
            "$message"
        );

        // And when a *hidden* event is the arrival-max, the shown arrival-max
        // wins instead of it.
        let mut page = vec![arrival_event("$msg", 100, 10)];
        page.push(state_event("$topic", 200, 99, "m.room.topic"));
        assert_eq!(
            read_targets_for(&page, &hidden, &nothing_promoted())
                .unwrap()
                .receipt
                .event_id,
            "$msg"
        );
        // With state events shown, that same event is a legal target.
        let shown = display_with_state_events(true);
        assert_eq!(
            read_targets_for(&page, &shown, &nothing_promoted())
                .unwrap()
                .receipt
                .event_id,
            "$topic"
        );
    }

    /// A thread reply the main timeline hides is not a receipt candidate, even
    /// as arrival-max.
    ///
    /// `should_show_event` alone lets one through — it never looks at
    /// `thread_relation()`, so `is_message_event()` passes any reply. The
    /// rendered timeline additionally filters on `thread_visible`, which hides a
    /// reply behind its root's badge until something promotes it. Two loads
    /// reach here with nothing promoted: a room's first ever (no marker, so
    /// `collect_unseen_thread_promotions` returns immediately), and any load
    /// where the reply is *backfilled* — promotion needs `origin_ts >
    /// marker_ts`, and a backfilled reply is old by `origin_ts` while being new
    /// by `arrival_order`. The second is this ADR's own scenario, which is why
    /// the numbers here are shaped like it. Review finding on #165.
    #[test]
    fn receipt_target_skips_an_unpromoted_thread_reply() {
        let display = display_with_state_events(false);
        let root = arrival_event("$root", 1_785_928_300_000, 1_871_400);
        // Backfilled: oldest by origin_ts, newest by arrival order.
        let mut reply = arrival_event("$reply", 1_785_928_200_000, 1_871_999);
        reply.relates_to = Some(serde_json::json!({ "rel_type": "m.thread", "event_id": "$root" }));
        let plain = arrival_event("$plain", 1_785_928_400_000, 1_871_500);
        let page = vec![root, reply, plain];

        assert_eq!(
            read_targets_for(&page, &display, &nothing_promoted())
                .unwrap()
                .receipt
                .event_id,
            "$plain",
            "an unpromoted thread reply is hidden from the main timeline, so it \
             cannot be receipted however recently it arrived"
        );

        // Once promoted it *is* on screen, and then it is the right answer.
        let promoted: std::collections::HashSet<String> = ["$reply".to_owned()].into();
        assert_eq!(
            read_targets_for(&page, &display, &promoted)
                .unwrap()
                .receipt
                .event_id,
            "$reply"
        );
    }

    /// The marker half of `receipt_target_skips_hidden_events` (#167).
    ///
    /// The load path used to take `page.events.last()` raw, one line below a
    /// receipt pick that filtered — so a trailing hidden state event became the
    /// marker even though the TUI never rendered it.
    #[test]
    fn marker_target_skips_hidden_events() {
        let hidden = display_with_state_events(false);
        let page = portal_page();

        // What the old code sent: the raw display-last, a bridge state event
        // that does not render at the default settings.
        assert_eq!(page.last().unwrap().event_id, "$bridge");
        assert_eq!(
            read_targets_for(&page, &hidden, &nothing_promoted())
                .unwrap()
                .marker
                .event_id,
            "$message"
        );

        // With state events shown it *is* rendered, so it is a legal marker.
        let shown = display_with_state_events(true);
        assert_eq!(
            read_targets_for(&page, &shown, &nothing_promoted())
                .unwrap()
                .marker
                .event_id,
            "$bridge"
        );
    }

    /// An unpromoted thread reply is hidden behind its root's badge, so it is
    /// no more a marker candidate than a receipt one — both picks read the same
    /// displayed set.
    #[test]
    fn marker_target_skips_an_unpromoted_thread_reply() {
        let display = display_with_state_events(false);
        let plain = arrival_event("$plain", 100, 1);
        let mut reply = arrival_event("$reply", 200, 2);
        reply.relates_to = Some(serde_json::json!({ "rel_type": "m.thread", "event_id": "$root" }));
        let page = vec![plain, reply];

        assert_eq!(
            read_targets_for(&page, &display, &nothing_promoted())
                .unwrap()
                .marker
                .event_id,
            "$plain"
        );

        let promoted: std::collections::HashSet<String> = ["$reply".to_owned()].into();
        assert_eq!(
            read_targets_for(&page, &display, &promoted)
                .unwrap()
                .marker
                .event_id,
            "$reply"
        );
    }

    #[test]
    fn read_targets_are_none_when_nothing_is_shown() {
        let hidden = display_with_state_events(false);
        let page = vec![
            state_event("$create", 100, 1, "m.room.create"),
            state_event("$topic", 200, 2, "m.room.topic"),
        ];
        // Nothing displayed → the load path advances neither position, rather
        // than falling back to the raw newest event. Both or neither is the
        // point of returning them together.
        assert!(read_targets_for(&page, &hidden, &nothing_promoted()).is_none());
        assert!(read_targets_for(&[], &hidden, &nothing_promoted()).is_none());
    }

    /// #167's named exposure — the reason filtering the marker is more than
    /// tidiness, and the consumer test guardrail 9 asks for.
    ///
    /// `collect_unseen_thread_promotions` treats anything at or before the
    /// marker as already seen. A marker over-advanced onto a hidden trailing
    /// state event therefore swallows a thread reply stamped between the last
    /// *visible* event and it: the badge that reply caused stays, but nothing
    /// on the main timeline explains why.
    #[test]
    fn a_filtered_marker_leaves_a_later_thread_reply_promotable() {
        let hidden = display_with_state_events(false);
        let promoted = nothing_promoted();

        // First load: one visible message, then a state event the default
        // config hides.
        let first_page = vec![
            arrival_event("$message", 100, 1),
            state_event("$bridge", 300, 2, "uk.half-shot.bridge"),
        ];
        assert_eq!(
            first_page.last().unwrap().origin_ts,
            300,
            "the raw display-last is the hidden event, which is the whole bug"
        );

        let targets = read_targets_for(&first_page, &hidden, &promoted).expect("a shown event");
        assert_eq!(targets.marker.origin_ts, 100);

        let mut app = app_with(vec![test_room("!r:x", 0)]);
        let k = key("!r:x");
        app.note_room_read(k.clone(), targets.marker, Some(targets.receipt));

        // A later load brings a thread reply stamped *between* the last visible
        // event and that hidden one. Against the filtered marker (100) it is
        // unseen and promotes; against the raw one (300) it never would.
        let reply = timeline_event("$reply", 200, Some("$root"));
        let roots = app.collect_unseen_thread_promotions(&k, std::slice::from_ref(&reply));
        assert_eq!(roots, vec![(Uuid::nil(), "$root".to_owned())]);
        assert!(app.promoted_thread_events.contains("$reply"));
    }

    #[test]
    fn thread_replies_past_the_marker_are_promoted_on_load() {
        let mut app = app_with(vec![test_room("!r:x", 500)]);
        let k = key("!r:x");
        app.read_markers.insert(
            k.clone(),
            ReadMarker {
                event_id: "$seen".to_owned(),
                origin_ts: 200,
            },
        );

        let page = vec![
            // Already read: at/behind the marker — never promoted.
            timeline_event("$old-reply", 150, Some("$root-a")),
            timeline_event("$at-marker", 200, Some("$root-a")),
            // Unseen thread replies — promoted; two on one root dedupe to one
            // root fetch. Own messages promote but never count as unread.
            timeline_event("$new-reply-1", 300, Some("$root-a")),
            timeline_event_from("@bob:example.com", "$new-reply-2", 400, Some("$root-a")),
            timeline_event_from("@bob:example.com", "$other-thread", 450, Some("$root-b")),
            // Unseen but not a thread member — nothing to promote.
            timeline_event("$plain", 500, None),
        ];
        let roots = app.collect_unseen_thread_promotions(&k, &page);

        assert!(app.promoted_thread_events.contains("$new-reply-1"));
        assert!(app.promoted_thread_events.contains("$new-reply-2"));
        assert!(app.promoted_thread_events.contains("$other-thread"));
        assert!(!app.promoted_thread_events.contains("$old-reply"));
        assert!(!app.promoted_thread_events.contains("$at-marker"));
        assert!(!app.promoted_thread_events.contains("$plain"));
        assert_eq!(
            roots,
            vec![
                (Uuid::nil(), "$root-a".to_owned()),
                (Uuid::nil(), "$root-b".to_owned()),
            ]
        );

        // The promoted replies now pass the main-timeline visibility filter.
        use crate::app::relations::thread_visible;
        assert!(thread_visible(
            &timeline_event("$new-reply-1", 300, Some("$root-a")),
            None,
            &app.promoted_thread_events,
        ));

        // Unseen replies from *others* also feed the thread-attention markers
        // (ADR 0049) — the load path covers what live observation missed while
        // this client was down. Own replies are promoted but never counted.
        let threads = app.unread_threads.get(&k).expect("thread markers");
        assert_eq!(threads.get("$root-a").map(|t| t.unread_count), Some(1));
        assert_eq!(threads.get("$root-b").map(|t| t.unread_count), Some(1));
        assert_eq!(
            threads.get("$root-a").map(|t| t.latest_event_id.as_str()),
            Some("$new-reply-2")
        );

        // Observing the same replies again (a reload while they are still past
        // the marker, or live-then-load) must not inflate the counts.
        app.collect_unseen_thread_promotions(&k, &page);
        let threads = app.unread_threads.get(&k).expect("thread markers");
        assert_eq!(threads.get("$root-a").map(|t| t.unread_count), Some(1));
        assert_eq!(threads.get("$root-b").map(|t| t.unread_count), Some(1));
    }

    #[test]
    fn rooms_without_a_marker_promote_nothing() {
        let mut app = app_with(vec![test_room("!r:x", 500)]);
        let k = key("!r:x");
        let page = vec![timeline_event("$reply", 300, Some("$root"))];
        let roots = app.collect_unseen_thread_promotions(&k, &page);
        assert!(roots.is_empty());
        assert!(app.promoted_thread_events.is_empty());
    }

    #[test]
    fn marker_value_roundtrip() {
        let marker = ReadMarker {
            event_id: "$e:example.com".to_owned(),
            origin_ts: 1234,
        };
        assert_eq!(marker_from_value(&marker_value(&marker)), Some(marker));
        assert_eq!(marker_from_value(&Value::Null), None);
        assert_eq!(
            marker_from_value(&serde_json::json!({ "event_id": "$e" })),
            None
        );
    }
}
