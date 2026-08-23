//! Cross-device draft sync over the M12 device-state API (ADR 0048).
//!
//! The compose buffer of each room is mirrored to Axon's per-device state
//! under the `drafts` namespace (key = room id, value `{"text": …}`, scoped by
//! `account_id`), so a draft typed on one device appears on the user's other
//! devices and survives a restart. The flow:
//!
//! - Keystrokes in [`Mode::Compose`](super::Mode) mark the current room's
//!   draft dirty ([`App::note_draft_activity`], called by the main loop after
//!   every key event); a debounce tick flushes it with one `PUT` per settled
//!   change ([`App::flush_due_draft_put`]). An **emptied** buffer (message
//!   sent, or text deleted) flushes as a `null` tombstone, so the clear wins
//!   the cross-device merge.
//! - Startup hydrates the local draft map from the server's merged view
//!   ([`App::apply_draft_reads`]). The buffer mirrors exactly one room's draft at
//!   a time ([`App::compose_room`]): switching rooms settles the outgoing
//!   room's draft and swaps in the newly-selected room's
//!   ([`App::sync_draft_on_room_change`]), and returning to compose from a
//!   transient mode that borrowed the buffer restores it
//!   ([`App::restore_draft_into_buffer`]).
//! - Live `device_state.changed` frames from *other* devices update the map
//!   as they arrive ([`App::handle_device_state_frame`]); frames carrying this
//!   client's own device id are its own PUTs echoed back and are dropped.
//!   The bus is lossy, so a reconnect re-reads the merged view instead of
//!   assuming the frames it missed.
//!
//! The visible compose buffer is only ever overwritten when it holds nothing
//! the user could lose: it must be empty or exactly equal to the room's last
//! known draft. A buffer mid-edit is never clobbered by a remote update.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::api::DeviceStateChangedDto;

use super::{App, LiveFrameAction, Mode, RoomKey, Status};

/// The device-state namespace drafts live under.
pub(crate) const DRAFTS_NAMESPACE: &str = "drafts";

/// How long the compose buffer must sit unchanged before its draft is PUT.
/// Long enough to collapse a typing burst into one request, short enough that
/// a sibling device sees the draft "within a second" (the M12 criterion).
pub(crate) const DRAFT_DEBOUNCE: Duration = Duration::from_millis(400);

/// A draft change waiting out its debounce window before being PUT.
pub(crate) struct PendingDraftPut {
    pub(crate) room: RoomKey,
    /// The settled draft text; `None` clears the draft (tombstone).
    pub(crate) value: Option<String>,
    pub(crate) due: Instant,
}

/// Result of a background draft PUT, drained by the main loop. Only failures
/// carry information — a successful PUT needs no UI reaction.
pub(crate) enum DraftOutcome {
    PutFailed(String),
}

/// Read this install's device UUID from `device-id` next to the config file,
/// minting and persisting one on first run. Any filesystem problem falls back
/// to a session-only random id (drafts still sync, identity just doesn't
/// persist) — a state file must never break startup.
pub(crate) fn load_or_create_device_id(config_path: &Path) -> Uuid {
    let Some(dir) = config_path.parent() else {
        return Uuid::new_v4();
    };
    let path = dir.join("device-id");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(id) = Uuid::parse_str(text.trim()) {
            return id;
        }
    }
    let id = Uuid::new_v4();
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&path, format!("{id}\n"));
    id
}

/// The draft value stored for a room's compose text: `{"text": …}`, leaving
/// room for future fields (e.g. a reply target) without a schema break.
fn draft_value(text: &str) -> Value {
    serde_json::json!({ "text": text })
}

/// Extract the compose text from a stored draft value.
fn draft_text(value: &Value) -> Option<&str> {
    value.get("text").and_then(Value::as_str)
}

impl App {
    /// Wire up the channel the main loop drains for background draft-PUT
    /// failures, plus this install's device id.
    pub(crate) fn set_drafts_sender(&mut self, tx: mpsc::UnboundedSender<DraftOutcome>) {
        self.drafts_tx = Some(tx);
    }

    /// Record this install's device id (from [`load_or_create_device_id`]).
    pub(crate) fn set_device_id(&mut self, device_id: Uuid) {
        self.device_id = device_id;
    }

    /// Called by the main loop after every key event: if the compose buffer
    /// diverged from the current room's known draft, (re)start the debounce
    /// window. A buffer holding a slash command in progress is not a draft
    /// (`//` — the literal-slash escape — still is).
    pub(crate) fn note_draft_activity(&mut self) {
        if self.mode != Mode::Compose {
            return;
        }
        let Some(room) = self.selected_room() else {
            return;
        };
        let room = RoomKey::from(room);
        // The buffer now belongs to this room; a room switch reconciles the
        // outgoing buffer against it ([`App::sync_draft_on_room_change`]).
        self.compose_room = Some(room.clone());
        let text = self.input.buffer.as_str();
        if text.starts_with('/') && !text.starts_with("//") {
            // A slash command in progress is not a draft (the `//` literal-slash
            // escape still is). Cancel any pending PUT for this room so a
            // half-typed command's text is never flushed once the buffer no
            // longer tracks a real draft.
            self.cancel_pending_draft_put(&room);
            // A command is not composing a message — clear any typing notice.
            self.stop_typing_for_room(&room);
            return;
        }
        let value = (!text.is_empty()).then(|| text.to_owned());
        if self.drafts.get(&room) == value.as_ref() {
            // Back in sync: nothing to PUT. An empty buffer here (typed then
            // cleared before the draft debounce ever stored anything) still
            // means the user isn't composing, so clear any typing notice. A
            // non-empty match is either an undo-to-synced or a just-restored
            // draft — not new composition — so the notice is left as it is (a
            // restore must not spuriously announce typing; a rare undo-then-idle
            // is cleared by the idle timeout).
            self.cancel_pending_draft_put(&room);
            if value.is_none() {
                self.stop_typing_for_room(&room);
            }
            return;
        }
        // The buffer diverged from the synced draft: the user is actively
        // editing. Drive the outbound typing notice off that (ADR 0068 M19a) —
        // an emptied buffer clears it, live text (re)starts it.
        match &value {
            Some(_) => self.note_typing(room.clone(), Instant::now()),
            None => self.stop_typing_for_room(&room),
        }
        self.pending_draft_put = Some(PendingDraftPut {
            room,
            value,
            due: Instant::now() + DRAFT_DEBOUNCE,
        });
    }

    /// Drop the pending draft PUT if it targets `room`. Used when the buffer
    /// falls back in sync with the known draft, or turns into a command.
    fn cancel_pending_draft_put(&mut self, room: &RoomKey) {
        if self
            .pending_draft_put
            .as_ref()
            .is_some_and(|p| &p.room == room)
        {
            self.pending_draft_put = None;
        }
    }

    /// Force the pending draft to flush now, regardless of its debounce window.
    /// Called when leaving a room (or handing the buffer to a transient mode)
    /// so the outgoing draft is settled before the buffer is repurposed.
    pub(crate) fn flush_pending_draft_now(&mut self) {
        if let Some(due) = self.pending_draft_put.as_ref().map(|p| p.due) {
            self.flush_due_draft_put(due);
        }
    }

    /// Called on the main-loop tick: flush the pending draft once its debounce
    /// window has passed. Applies the change to the local map immediately and
    /// PUTs it in the background (server-clock LWW — no response data needed).
    pub(crate) fn flush_due_draft_put(&mut self, now: Instant) {
        if self.pending_draft_put.as_ref().is_none_or(|p| now < p.due) {
            return;
        }
        let Some(pending) = self.pending_draft_put.take() else {
            return;
        };
        match &pending.value {
            Some(text) => {
                self.drafts.insert(pending.room.clone(), text.clone());
            }
            None => {
                // An empty buffer for a room that never had a draft needs no
                // tombstone; `remove` reporting the key was absent tells us so.
                if self.drafts.remove(&pending.room).is_none() {
                    return;
                }
            }
        }

        // Not wired (unit tests): the flush stays local-only, like the other
        // background channels.
        let Some(tx) = self.drafts_tx.clone() else {
            return;
        };
        let client = self.client.clone();
        let device_id = self.device_id;
        tokio::spawn(async move {
            let entries: HashMap<String, Option<Value>> = HashMap::from([(
                pending.room.room_id.clone(),
                pending.value.as_deref().map(draft_value),
            )]);
            if let Err(err) = client
                .put_device_state(
                    device_id,
                    pending.room.account_id,
                    DRAFTS_NAMESPACE,
                    &entries,
                )
                .await
            {
                let _ = tx.send(DraftOutcome::PutFailed(err.to_string()));
            }
        });
    }

    /// Surface a background draft-PUT failure. Quietly (a Debug status): the
    /// draft is still intact locally and the next change retries naturally.
    pub(crate) fn handle_draft_outcome(&mut self, outcome: DraftOutcome) {
        let DraftOutcome::PutFailed(err) = outcome;
        if !self.is_mid_command() {
            self.status = Status::Debug(format!("draft sync failed: {err}"));
        }
    }

    /// Apply the merged draft view for every account, then restore the current
    /// room's draft into the compose buffer.
    ///
    /// The fetch half lives in [`super::bootstrap`] so startup and every WS
    /// (re)connect can run it off the event loop (#189).
    pub(crate) fn apply_draft_reads(&mut self, reads: super::bootstrap::DeviceStateReads) {
        for (account_id, result) in reads {
            let Ok(state) = result else {
                continue;
            };
            // The merged view is authoritative for this account: keys it omits
            // were tombstoned server-side (possibly while we were offline), so
            // drop the stale local entries or they'd resurrect on the next room
            // entry. A room with an unsettled local edit (the current compose
            // room, or one with a pending PUT) is kept — our own write just
            // hasn't round-tripped yet, so the server's older view isn't newer.
            let present: std::collections::HashSet<&str> =
                state.entries.keys().map(String::as_str).collect();
            // Plus anything a live frame wrote after this read was dispatched:
            // that write is newer than the view being applied, so the omission
            // is staleness in the fetch, not a server-side tombstone.
            let protected: Vec<&RoomKey> = self
                .compose_room
                .iter()
                .chain(self.pending_draft_put.as_ref().map(|p| &p.room))
                .chain(self.drafts_written_since_fetch.iter())
                .filter(|k| k.account_id == account_id)
                .collect();
            self.drafts.retain(|key, _| {
                key.account_id != account_id
                    || present.contains(key.room_id.as_str())
                    || protected.contains(&key)
            });
            for (room_id, entry) in state.entries {
                let key = RoomKey {
                    account_id,
                    room_id,
                };
                // `retain` above stops a stale view *deleting* a protected key;
                // without this the insert below would overwrite it instead,
                // which loses exactly the same write by the other door. Applies
                // to all three protected kinds: a live frame's newer text, the
                // compose room, and a draft whose PUT has not round-tripped.
                if protected.contains(&&key) {
                    continue;
                }
                if let Some(text) = draft_text(&entry.value) {
                    self.drafts.insert(key, text.to_owned());
                }
            }
        }
        self.restore_draft_into_buffer();
    }

    /// True while the compose buffer holds a room draft — i.e. no transient
    /// input mode (login, edit, react, date-jump, …) has borrowed it for its
    /// own text. Draft save/restore only touches the buffer in these states.
    pub(crate) fn buffer_holds_draft(&self) -> bool {
        !matches!(
            self.mode,
            Mode::LoginUsername
                | Mode::LoginPassword { .. }
                | Mode::RecoveryKey { .. }
                | Mode::ConfirmLogout { .. }
                | Mode::ConfirmDelete { .. }
                | Mode::Editing { .. }
                | Mode::Reacting { .. }
                | Mode::Unreacting { .. }
                | Mode::Verification
                | Mode::DateJump
        )
    }

    /// Load the current room's draft into the compose buffer, when doing so
    /// cannot lose anything: only while the buffer holds a draft (no transient
    /// mode has borrowed it) and only over an empty buffer. Called after
    /// hydration and when returning from a transient mode that borrowed (and
    /// cleared) the buffer — including exits that land outside `Mode::Compose`
    /// (e.g. date-jump, which returns to `Mode::MessageList`).
    pub(crate) fn restore_draft_into_buffer(&mut self) {
        if !self.buffer_holds_draft() || !self.input.buffer.is_empty() {
            return;
        }
        let Some(room) = self.selected_room() else {
            return;
        };
        let key = RoomKey::from(room);
        if let Some(text) = self.drafts.get(&key) {
            self.input.buffer = text.clone();
            self.input.cursor = self.input.buffer.len();
        }
        self.compose_room = Some(key);
    }

    /// Reconcile the compose buffer when the selected room changes. The buffer
    /// mirrors exactly one room's draft ([`Self::compose_room`]); on a switch,
    /// settle the outgoing room's pending PUT (so a mid-debounce switch can't
    /// drop it or misattribute it to the new room) and swap the buffer to the
    /// newly-selected room's draft. Called at the end of a room load.
    pub(crate) fn sync_draft_on_room_change(&mut self) {
        if !self.buffer_holds_draft() {
            return;
        }
        let selected = self.selected_room().map(RoomKey::from);
        if self.compose_room == selected {
            // Same room (e.g. a timeline reload): leave the live buffer alone.
            return;
        }
        // Leaving the previous room: stop telling its peers we're typing.
        self.stop_typing_now();
        // Settle the previous room's draft before leaving it behind.
        self.flush_pending_draft_now();
        self.input.buffer.clear();
        if let Some(key) = &selected {
            if let Some(text) = self.drafts.get(key) {
                self.input.buffer = text.clone();
            }
        }
        self.input.cursor = self.input.buffer.len();
        self.compose_room = selected;
    }

    /// Route a live `device_state.changed` frame to its namespace's handler.
    /// Our own frames (matching `device_id`) are echoes of our own PUTs and
    /// are dropped here for every namespace; namespaces this client doesn't
    /// consume are ignored (forward-compatibility).
    pub(crate) fn handle_device_state_frame(
        &mut self,
        account_id: Uuid,
        payload: DeviceStateChangedDto,
    ) -> LiveFrameAction {
        if payload.device_id == self.device_id {
            return LiveFrameAction::None;
        }
        match payload.namespace.as_str() {
            DRAFTS_NAMESPACE => self.handle_draft_frame(account_id, payload.entries),
            super::read_markers::READ_MARKERS_NAMESPACE => {
                self.handle_read_marker_frame(account_id, payload.entries)
            }
            _ => LiveFrameAction::None,
        }
    }

    /// Apply a live `drafts` entry map from a sibling device. The visible
    /// compose buffer is updated only when it holds nothing the user could
    /// lose — empty, or exactly the draft value being replaced.
    fn handle_draft_frame(
        &mut self,
        account_id: Uuid,
        entries: HashMap<String, Value>,
    ) -> LiveFrameAction {
        for (room_id, value) in entries {
            let key = RoomKey {
                account_id,
                room_id,
            };
            let previous = self.drafts.get(&key).cloned();
            let incoming = if value.is_null() {
                None
            } else {
                match draft_text(&value) {
                    Some(text) => Some(text.to_owned()),
                    // An unrecognized draft shape (a future client?) — leave
                    // local state alone rather than misrender it.
                    None => continue,
                }
            };

            match &incoming {
                Some(text) => {
                    self.drafts.insert(key.clone(), text.clone());
                }
                None => {
                    self.drafts.remove(&key);
                }
            }
            // Newer than any read already in flight; see `apply_draft_reads`.
            self.drafts_written_since_fetch.insert(key.clone());

            // Reflect the change in the visible buffer when it currently mirrors
            // this room's draft and holds no unsynced local edit. Keying on
            // `compose_room` (not the live mode) means a clean buffer is kept
            // fresh even while focus is elsewhere (room list, message list) — so
            // returning to compose never reverts a newer sibling draft — while a
            // transient mode that borrowed the buffer (edit/react) is left be.
            if self.compose_room.as_ref() != Some(&key) || !self.buffer_holds_draft() {
                continue;
            }
            let buffer_clean = self.input.buffer.is_empty()
                || previous.as_deref() == Some(self.input.buffer.as_str());
            if !buffer_clean {
                continue;
            }
            self.input.buffer = incoming.unwrap_or_default();
            self.input.cursor = self.input.buffer.len();
        }
        LiveFrameAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AxonClient, RoomDto};
    use crate::config::TuiConfig;
    use ratatui_image::picker::Picker;

    fn test_room(account_id: Uuid, room_id: &str) -> RoomDto {
        RoomDto {
            account_id,
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: room_id.to_owned(),
            name: Some("Room".to_owned()),
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            last_activity_ts: 0,
            last_event_id: None,
        }
    }

    /// An `App` in `Mode::Compose` with one selected room and a device id.
    fn compose_app(room: &RoomDto) -> App {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.rooms.rooms = vec![room.clone()];
        app.rooms.selected = Some(0);
        app.compose_room = Some(RoomKey::from(room));
        app.set_device_id(Uuid::new_v4());
        app
    }

    fn frame(device_id: Uuid, entries: Vec<(&str, Value)>) -> DeviceStateChangedDto {
        DeviceStateChangedDto {
            device_id,
            namespace: DRAFTS_NAMESPACE.to_owned(),
            entries: entries
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        }
    }

    /// The same protection, against the other door.
    ///
    /// When the stale view *omits* the room, `retain` keeps the newer local
    /// value. When it *contains* the room with older text, `retain` keeps it
    /// too — and the insert loop then has to not overwrite it. Protecting only
    /// the delete path loses the identical write via `insert`.
    #[test]
    fn a_live_draft_is_not_overwritten_by_older_text_in_the_same_fetch() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);
        app.compose_room = None;

        // The read goes out; at that moment the server has "v1" for this room.
        app.request_device_state();

        // While it is in flight, a sibling device moves it to "v2".
        let other_device = Uuid::new_v4();
        app.handle_device_state_frame(
            Uuid::nil(),
            frame(other_device, vec![("!r:example.com", draft_value("v2"))]),
        );

        // The older read lands, carrying "v1" for that very room.
        let mut entries = HashMap::new();
        entries.insert(
            "!r:example.com".to_owned(),
            crate::api::DeviceStateEntryDto {
                value: draft_value("v1"),
            },
        );
        app.apply_draft_reads(vec![(
            Uuid::nil(),
            Ok(crate::api::DeviceStateDto { entries }),
        )]);

        assert_eq!(
            app.drafts.get(&key).map(String::as_str),
            Some("v2"),
            "a write newer than the fetch must survive the fetch's own older text"
        );
    }

    /// A draft a live frame installed while a device-state read was in flight
    /// must survive that read landing.
    ///
    /// `apply_draft_reads` treats the merged view as authoritative and drops
    /// keys it omits — correct for a server-side tombstone, wrong for a write
    /// that happened *after* the read was dispatched. Reachable since the read
    /// became a spawn (#189): the old awaited call blocked the loop, so no
    /// frame could be applied mid-flight.
    #[test]
    fn a_live_draft_written_during_a_fetch_survives_that_fetch() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);
        // Not the compose room, so the existing `protected` set does not cover
        // it — this must be saved by the newer-than-the-fetch rule alone.
        app.compose_room = None;

        // A read goes out; the server's view at that moment has no draft here.
        app.request_device_state();

        // While it is in flight, a sibling device's frame installs one.
        let other_device = Uuid::new_v4();
        app.handle_device_state_frame(
            Uuid::nil(),
            frame(
                other_device,
                vec![("!r:example.com", draft_value("from my phone"))],
            ),
        );
        assert_eq!(
            app.drafts.get(&key).map(String::as_str),
            Some("from my phone")
        );

        // The older read lands, omitting the room.
        app.apply_draft_reads(vec![(
            Uuid::nil(),
            Ok(crate::api::DeviceStateDto {
                entries: HashMap::new(),
            }),
        )]);

        assert_eq!(
            app.drafts.get(&key).map(String::as_str),
            Some("from my phone"),
            "a write newer than the fetch is not a server-side tombstone"
        );
    }

    #[test]
    fn typing_arms_the_debounce_and_flush_updates_the_map() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);

        app.input.buffer = "hello".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_some(), "divergence arms debounce");

        // Not due yet: nothing flushes.
        app.flush_due_draft_put(Instant::now());
        assert!(app.pending_draft_put.is_some());
        assert!(app.drafts.is_empty());

        // Due: the local map reflects the settled draft. (The background PUT
        // is spawned fire-and-forget; local state is what we can observe.)
        app.flush_due_draft_put(Instant::now() + DRAFT_DEBOUNCE);
        assert!(app.pending_draft_put.is_none());
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("hello"));

        // Emptying the buffer (send/clear) flushes as a removal.
        app.input.buffer.clear();
        app.note_draft_activity();
        app.flush_due_draft_put(Instant::now() + DRAFT_DEBOUNCE);
        assert!(app.drafts.is_empty(), "cleared draft leaves the map");
    }

    #[test]
    fn commands_and_undone_edits_do_not_sync() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let mut app = compose_app(&room);

        // A slash command in progress is not a draft…
        app.input.buffer = "/room 2".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_none());

        // …but the `//` literal-slash escape is.
        app.input.buffer = "//actual message".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_some());
        app.pending_draft_put = None;

        // Typing then undoing back to the synced value cancels the pending PUT.
        let key = RoomKey::from(&room);
        app.drafts.insert(key, "kept".to_owned());
        app.input.buffer = "kept but edited".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_some());
        app.input.buffer = "kept".to_owned();
        app.note_draft_activity();
        assert!(
            app.pending_draft_put.is_none(),
            "back in sync: nothing to do"
        );
    }

    #[test]
    fn remote_frame_updates_map_and_clean_buffer_only() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);
        let other_device = Uuid::new_v4();

        // Empty buffer: the remote draft lands in the map and the buffer.
        app.handle_device_state_frame(
            key.account_id,
            frame(
                other_device,
                vec![("!r:example.com", draft_value("from B"))],
            ),
        );
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("from B"));
        assert_eq!(app.input.buffer, "from B");
        assert_eq!(app.input.cursor, app.input.buffer.len());

        // A buffer matching the known draft follows a remote update…
        app.handle_device_state_frame(
            key.account_id,
            frame(other_device, vec![("!r:example.com", draft_value("newer"))]),
        );
        assert_eq!(app.input.buffer, "newer");

        // …but a locally diverged buffer is never clobbered.
        app.input.buffer = "my local edit".to_owned();
        app.handle_device_state_frame(
            key.account_id,
            frame(
                other_device,
                vec![("!r:example.com", draft_value("remote"))],
            ),
        );
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("remote"));
        assert_eq!(app.input.buffer, "my local edit");

        // A remote clear empties a clean buffer.
        app.input.buffer = "remote".to_owned();
        app.handle_device_state_frame(
            key.account_id,
            frame(other_device, vec![("!r:example.com", Value::Null)]),
        );
        assert!(app.drafts.is_empty());
        assert!(app.input.buffer.is_empty());
    }

    #[test]
    fn own_echo_and_foreign_namespaces_are_ignored() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);

        // Our own PUT echoed back must not touch anything.
        app.handle_device_state_frame(
            key.account_id,
            frame(app.device_id, vec![("!r:example.com", draft_value("echo"))]),
        );
        assert!(app.drafts.is_empty());
        assert!(app.input.buffer.is_empty());

        // A namespace this client doesn't consume is ignored.
        let mut other = frame(Uuid::new_v4(), vec![("!r:example.com", draft_value("x"))]);
        other.namespace = "read_markers".to_owned();
        app.handle_device_state_frame(key.account_id, other);
        assert!(app.drafts.is_empty());
    }

    #[test]
    fn switching_into_a_room_restores_its_draft_into_an_empty_buffer() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);
        app.drafts.insert(key, "stored draft".to_owned());

        app.restore_draft_into_buffer();
        assert_eq!(app.input.buffer, "stored draft");

        // A non-empty buffer is never overwritten by a restore.
        app.input.buffer = "typing something".to_owned();
        app.restore_draft_into_buffer();
        assert_eq!(app.input.buffer, "typing something");
    }

    #[test]
    fn switching_rooms_settles_the_old_draft_and_loads_the_new() {
        let a = test_room(Uuid::nil(), "!a:example.com");
        let b = test_room(Uuid::nil(), "!b:example.com");
        let key_a = RoomKey::from(&a);
        let key_b = RoomKey::from(&b);
        let mut app = compose_app(&a);
        app.rooms.rooms = vec![a.clone(), b.clone()];
        app.rooms.selected = Some(0);
        app.compose_room = Some(key_a.clone());
        app.drafts.insert(key_b.clone(), "draft in B".to_owned());

        // Type in A; the debounce arms but hasn't fired yet.
        app.input.buffer = "draft in A".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_some());

        // Switch to B before the debounce fires.
        app.rooms.selected = Some(1);
        app.sync_draft_on_room_change();

        // A's in-flight draft was settled into the map, not dropped, and the
        // buffer now shows B's draft.
        assert_eq!(
            app.drafts.get(&key_a).map(String::as_str),
            Some("draft in A")
        );
        assert!(app.pending_draft_put.is_none());
        assert_eq!(app.input.buffer, "draft in B");
        assert_eq!(app.compose_room, Some(key_b.clone()));

        // Editing B's draft never touches A's.
        app.input.buffer = "draft in B!".to_owned();
        app.note_draft_activity();
        app.flush_due_draft_put(Instant::now() + DRAFT_DEBOUNCE);
        assert_eq!(
            app.drafts.get(&key_a).map(String::as_str),
            Some("draft in A")
        );
        assert_eq!(
            app.drafts.get(&key_b).map(String::as_str),
            Some("draft in B!")
        );
    }

    #[test]
    fn remote_frame_refreshes_a_clean_buffer_even_when_focus_is_elsewhere() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);
        let other = Uuid::new_v4();
        app.drafts.insert(key.clone(), "hello".to_owned());
        app.input.buffer = "hello".to_owned();

        // Focus has moved to the room list; the buffer still mirrors the draft.
        app.mode = Mode::RoomList;
        app.handle_device_state_frame(
            key.account_id,
            frame(other, vec![("!r:example.com", draft_value("hello world"))]),
        );
        // The clean buffer follows the sibling's newer draft, so returning to
        // compose can't revert it.
        assert_eq!(app.input.buffer, "hello world");
        assert_eq!(
            app.drafts.get(&key).map(String::as_str),
            Some("hello world")
        );

        // But a transient mode that borrowed the buffer is never clobbered.
        app.mode = Mode::Editing {
            event_id: "$e:example.com".to_owned(),
        };
        app.input.buffer = "message being edited".to_owned();
        app.handle_device_state_frame(
            key.account_id,
            frame(other, vec![("!r:example.com", draft_value("even newer"))]),
        );
        assert_eq!(app.input.buffer, "message being edited");
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("even newer"));
    }

    #[test]
    fn returning_from_a_borrowed_buffer_restores_the_draft_not_a_tombstone() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);
        app.drafts.insert(key.clone(), "my draft".to_owned());
        app.input.buffer = "my draft".to_owned();
        app.compose_room = Some(key.clone());

        // A transient mode (edit/react) settles the draft and clears the buffer.
        app.flush_pending_draft_now();
        app.mode = Mode::Editing {
            event_id: "$e:example.com".to_owned(),
        };
        app.input.buffer = "loaded message body".to_owned();

        // On exit the buffer is emptied and compose is restored.
        app.input.buffer.clear();
        app.mode = Mode::Compose;
        app.restore_draft_into_buffer();
        assert_eq!(app.input.buffer, "my draft");

        // The very next activity check sees buffer == draft: no tombstone armed.
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_none());
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("my draft"));
    }

    #[test]
    fn date_jump_borrow_settles_and_restores_the_draft() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let key = RoomKey::from(&room);
        let mut app = compose_app(&room);

        // Type a draft; the debounce is armed but hasn't flushed to the map yet.
        app.input.buffer = "my draft".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_some());
        assert!(app.drafts.is_empty());

        // Entering date-jump settles the pending draft, then borrows the buffer
        // (mirrors start_date_jump's flush_pending_draft_now + clear_input_buffer).
        app.flush_pending_draft_now();
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("my draft"));
        app.input.buffer.clear();
        app.mode = Mode::DateJump;
        app.input.buffer = "2026-01-15".to_owned();

        // The date-jump exit lands in MessageList, not Compose; restore still
        // refills the borrowed buffer from the saved draft.
        app.input.buffer.clear();
        app.mode = Mode::MessageList;
        app.restore_draft_into_buffer();
        assert_eq!(app.input.buffer, "my draft");
        assert_eq!(app.compose_room, Some(key.clone()));

        // Back in compose, the next activity check sees buffer == draft: no
        // tombstone armed, so the draft survives the detour.
        app.mode = Mode::Compose;
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_none());
        assert_eq!(app.drafts.get(&key).map(String::as_str), Some("my draft"));
    }

    #[test]
    fn a_command_buffer_cancels_a_pending_draft_put() {
        let room = test_room(Uuid::nil(), "!r:example.com");
        let mut app = compose_app(&room);
        app.input.buffer = "hello".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_some());

        // Turning the buffer into a slash command cancels the pending PUT so
        // the half-typed command text is never flushed as a draft.
        app.input.buffer = "/room other".to_owned();
        app.note_draft_activity();
        assert!(app.pending_draft_put.is_none());
    }

    #[test]
    fn device_id_persists_across_loads() {
        let dir = std::env::temp_dir().join(format!("axon-tui-drafts-test-{}", Uuid::new_v4()));
        let config_path = dir.join("config.toml");
        let first = load_or_create_device_id(&config_path);
        let second = load_or_create_device_id(&config_path);
        assert_eq!(first, second, "the minted id is re-read, not re-minted");
        let on_disk = std::fs::read_to_string(dir.join("device-id")).expect("state file");
        assert_eq!(on_disk.trim(), first.to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn draft_value_roundtrip() {
        let value = draft_value("hello there");
        assert_eq!(draft_text(&value), Some("hello there"));
        // Unknown shapes are rejected, not misread.
        assert_eq!(draft_text(&serde_json::json!({ "other": 1 })), None);
        assert_eq!(draft_text(&serde_json::json!("bare string")), None);
    }
}
