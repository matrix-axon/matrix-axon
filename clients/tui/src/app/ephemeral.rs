//! Inbound Matrix ephemeral overlays (M18, ADR 0056): typing notices and read
//! receipts the server forwards as `ephemeral.passthrough` frames. This is the
//! consumer the TUI previously lacked (the web client's `stores/ephemeral.ts`
//! is the reference); the raw Matrix `content` is parsed here, never persisted.
//!
//! Typing is live-only and self-heals: each notice carries a [`TYPING_TTL`] and
//! is pruned on the main-loop tick, and every notice is cleared on reconnect
//! (the bus is lossy — a "stopped typing" frame may have been missed). Receipts
//! track the newest event each user has publicly read (`m.read`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;

use crate::api::EphemeralPassthroughDto;

use super::{App, RoomKey};

/// A typing notice is shown at most this long before it is pruned as stale.
/// Matrix clients refresh well within this; a missed stop self-heals here.
pub(crate) const TYPING_TTL: Duration = Duration::from_secs(30);

struct TypingEntry {
    user_ids: Vec<String>,
    expires_at: Instant,
}

/// The newest event a user has publicly read.
struct ReadAt {
    event_id: String,
}

/// Live typing + read-receipt overlays, keyed by room.
#[derive(Default)]
pub(crate) struct EphemeralState {
    typing: HashMap<RoomKey, TypingEntry>,
    /// Per room: user id -> the newest event they've publicly read.
    read: HashMap<RoomKey, HashMap<String, ReadAt>>,
}

/// Parse an `m.typing` content (`{ "user_ids": ["@a:hs", …] }`), deduplicated.
/// `None` for a shape this client doesn't recognize (left alone, not misread).
fn parse_typing(content: &Value) -> Option<Vec<String>> {
    let array = content.get("user_ids")?.as_array()?;
    let mut users = Vec::with_capacity(array.len());
    for value in array {
        let user = value.as_str()?;
        if !users.iter().any(|existing| existing == user) {
            users.push(user.to_owned());
        }
    }
    Some(users)
}

/// Parse an `m.receipt` content into (event_id, user_id) public-read pairs:
/// `{ "$evt": { "m.read": { "@u:hs": { "ts": 123 } } } }`. Private-read markers
/// (`m.read.private`) are the user's own and are not surfaced to others.
fn parse_public_reads(content: &Value) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let Some(events) = content.as_object() else {
        return pairs;
    };
    for (event_id, receipts) in events {
        let Some(readers) = receipts.get("m.read").and_then(Value::as_object) else {
            continue;
        };
        for user_id in readers.keys() {
            pairs.push((event_id.clone(), user_id.clone()));
        }
    }
    pairs
}

impl App {
    /// Apply one `ephemeral.passthrough` frame. `now` sets typing expiry.
    pub(crate) fn handle_ephemeral_frame(
        &mut self,
        account_id: Uuid,
        payload: EphemeralPassthroughDto,
        now: Instant,
    ) {
        // Account-scoped signals (a future presence frame) have no room; the
        // TUI surfaces only room-scoped overlays today.
        let Some(room_id) = payload.room_id else {
            return;
        };
        let room = RoomKey {
            account_id,
            room_id,
        };
        match payload.event_type.as_str() {
            "m.typing" => {
                let Some(users) = parse_typing(&payload.content) else {
                    return;
                };
                if users.is_empty() {
                    self.ephemeral.typing.remove(&room);
                } else {
                    self.ephemeral.typing.insert(
                        room,
                        TypingEntry {
                            user_ids: users,
                            expires_at: now + TYPING_TTL,
                        },
                    );
                }
            }
            "m.receipt" => {
                let pairs = parse_public_reads(&payload.content);
                if pairs.is_empty() {
                    return;
                }
                let room_read = self.ephemeral.read.entry(room).or_default();
                for (event_id, user_id) in pairs {
                    room_read.insert(user_id, ReadAt { event_id });
                }
            }
            // An allowlisted type this client doesn't render yet: ignore
            // (forward-compatibility, matching the frame decoder).
            _ => {}
        }
    }

    /// Main-loop tick: drop typing notices whose TTL has elapsed.
    pub(crate) fn prune_typing(&mut self, now: Instant) {
        self.ephemeral
            .typing
            .retain(|_, entry| entry.expires_at > now);
    }

    /// Drop every typing overlay (called on (re)connect — see
    /// [`super::timeline`]).
    pub(crate) fn clear_typing_overlays(&mut self) {
        self.ephemeral.typing.clear();
    }

    /// A combined bottom-status line for the selected room: who is typing, and
    /// who has read the user's latest message. `None` when there is neither.
    pub(crate) fn ephemeral_status_line(&self) -> Option<String> {
        let room = self.selected_room().map(RoomKey::from)?;
        let own = self.own_user_id(room.account_id);
        let mut parts: Vec<String> = Vec::new();
        if let Some(typing) = self.typing_indicator(&room, own.as_deref()) {
            parts.push(typing);
        }
        if let Some(seen) = self.receipt_indicator(&room, own.as_deref()) {
            parts.push(seen);
        }
        (!parts.is_empty()).then(|| parts.join("   "))
    }

    /// Own user id for an account (to exclude the user from their own overlays).
    fn own_user_id(&self, account_id: Uuid) -> Option<String> {
        self.live.own_senders.get(&account_id).cloned().or_else(|| {
            self.accounts
                .accounts
                .iter()
                .find(|a| a.account_id == account_id)
                .map(|a| a.user_id.clone())
        })
    }

    fn typing_indicator(&self, room: &RoomKey, own: Option<&str>) -> Option<String> {
        let entry = self.ephemeral.typing.get(room)?;
        let names: Vec<String> = entry
            .user_ids
            .iter()
            .filter(|user| Some(user.as_str()) != own)
            .map(|user| self.ephemeral_display_name(room, user))
            .collect();
        match names.as_slice() {
            [] => None,
            [a] => Some(format!("{a} is typing…")),
            [a, b] => Some(format!("{a} and {b} are typing…")),
            [a, b, rest @ ..] => Some(format!("{a}, {b}, and {} more are typing…", rest.len())),
        }
    }

    fn receipt_indicator(&self, room: &RoomKey, own: Option<&str>) -> Option<String> {
        let own = own?;
        let events = self.messages.events.get(room)?;
        // The user's newest own message in the loaded window; only its read
        // state is worth surfacing.
        let last_own = events.iter().rev().find(|event| event.sender == own)?;
        let room_read = self.ephemeral.read.get(room)?;
        // A reader has seen the message if their read position is at or past it.
        // Positions are compared by the loaded events' `origin_ts`; a receipt
        // pointing at an event outside the loaded window is skipped rather than
        // guessed at.
        let event_ts: HashMap<&str, i64> = events
            .iter()
            .map(|event| (event.event_id.as_str(), event.origin_ts))
            .collect();
        let mut names: Vec<String> = room_read
            .iter()
            .filter(|(user, _)| user.as_str() != own)
            .filter(|(_, at)| {
                event_ts
                    .get(at.event_id.as_str())
                    .is_some_and(|&ts| ts >= last_own.origin_ts)
            })
            .map(|(user, _)| self.ephemeral_display_name(room, user))
            .collect();
        if names.is_empty() {
            return None;
        }
        names.sort();
        Some(format!("✓ read by {}", names.join(", ")))
    }

    /// Resolve a user id to its room display name, falling back to the id.
    fn ephemeral_display_name(&self, room: &RoomKey, user_id: &str) -> String {
        self.rooms
            .display_names
            .get(room)
            .and_then(|names| names.get(user_id))
            .filter(|name| !name.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| user_id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AxonClient, EventDto, RoomDto};
    use crate::config::TuiConfig;
    use ratatui_image::picker::Picker;

    fn app_with_room() -> App {
        let mut app = App::new(
            AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            Picker::halfblocks(),
        );
        app.rooms.rooms = vec![RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@me:hs".to_owned()),
            room_id: "!r:hs".to_owned(),
            name: Some("Room".to_owned()),
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            last_activity_ts: 0,
            last_event_id: None,
        }];
        app.rooms.selected = Some(0);
        app.seed_own_senders_from_rooms();
        app
    }

    fn room() -> RoomKey {
        RoomKey {
            account_id: Uuid::nil(),
            room_id: "!r:hs".to_owned(),
        }
    }

    fn typing_frame(user_ids: &[&str]) -> EphemeralPassthroughDto {
        EphemeralPassthroughDto {
            room_id: Some("!r:hs".to_owned()),
            event_type: "m.typing".to_owned(),
            content: serde_json::json!({ "user_ids": user_ids }),
        }
    }

    fn event(sender: &str, event_id: &str, origin_ts: i64) -> EventDto {
        EventDto {
            account_id: Uuid::nil(),
            event_id: event_id.to_owned(),
            room_id: "!r:hs".to_owned(),
            sender: sender.to_owned(),
            state_key: None,
            arrival_order: origin_ts,
            origin_ts,
            event_type: "m.room.message".to_owned(),
            content: Some(serde_json::json!({ "msgtype": "m.text", "body": "hi" })),
            body: Some("hi".to_owned()),
            relates_to: None,
            redacted: false,
            redaction_event_id: None,
            reactions: None,
            sender_trust: None,
        }
    }

    #[test]
    fn typing_notice_shows_others_and_excludes_self() {
        let mut app = app_with_room();
        let now = Instant::now();
        app.handle_ephemeral_frame(Uuid::nil(), typing_frame(&["@me:hs", "@bob:hs"]), now);
        assert_eq!(
            app.ephemeral_status_line().as_deref(),
            Some("@bob:hs is typing…")
        );

        // An empty user list clears the notice.
        app.handle_ephemeral_frame(Uuid::nil(), typing_frame(&[]), now);
        assert_eq!(app.ephemeral_status_line(), None);
    }

    #[test]
    fn typing_uses_known_display_names_and_pluralizes() {
        let mut app = app_with_room();
        app.rooms.display_names.insert(
            room(),
            HashMap::from([
                ("@bob:hs".to_owned(), "Bob".to_owned()),
                ("@carol:hs".to_owned(), "Carol".to_owned()),
            ]),
        );
        app.handle_ephemeral_frame(
            Uuid::nil(),
            typing_frame(&["@bob:hs", "@carol:hs"]),
            Instant::now(),
        );
        assert_eq!(
            app.ephemeral_status_line().as_deref(),
            Some("Bob and Carol are typing…")
        );
    }

    #[test]
    fn typing_is_pruned_after_its_ttl() {
        let mut app = app_with_room();
        let now = Instant::now();
        app.handle_ephemeral_frame(Uuid::nil(), typing_frame(&["@bob:hs"]), now);
        app.prune_typing(now + TYPING_TTL - Duration::from_millis(1));
        assert!(app.ephemeral_status_line().is_some(), "still within TTL");
        app.prune_typing(now + TYPING_TTL);
        assert_eq!(app.ephemeral_status_line(), None, "pruned past TTL");
    }

    #[test]
    fn reconnect_clears_typing() {
        let mut app = app_with_room();
        app.handle_ephemeral_frame(Uuid::nil(), typing_frame(&["@bob:hs"]), Instant::now());
        app.clear_typing_overlays();
        assert_eq!(app.ephemeral_status_line(), None);
    }

    #[test]
    fn receipt_shows_readers_at_or_past_the_users_last_message() {
        let mut app = app_with_room();
        app.messages.events.insert(
            room(),
            vec![
                event("@me:hs", "$mine", 100),
                event("@bob:hs", "$later", 200),
            ],
        );
        // Bob's read marker is on the later event → he has seen $mine.
        app.handle_ephemeral_frame(
            Uuid::nil(),
            EphemeralPassthroughDto {
                room_id: Some("!r:hs".to_owned()),
                event_type: "m.receipt".to_owned(),
                content: serde_json::json!({ "$later": { "m.read": { "@bob:hs": { "ts": 5 } } } }),
            },
            Instant::now(),
        );
        assert_eq!(
            app.ephemeral_status_line().as_deref(),
            Some("✓ read by @bob:hs")
        );
    }

    #[test]
    fn receipt_before_the_users_last_message_is_not_shown() {
        let mut app = app_with_room();
        app.messages.events.insert(
            room(),
            vec![
                event("@bob:hs", "$early", 100),
                event("@me:hs", "$mine", 200),
            ],
        );
        // Bob only read the earlier event → he has not seen $mine.
        app.handle_ephemeral_frame(
            Uuid::nil(),
            EphemeralPassthroughDto {
                room_id: Some("!r:hs".to_owned()),
                event_type: "m.receipt".to_owned(),
                content: serde_json::json!({ "$early": { "m.read": { "@bob:hs": { "ts": 5 } } } }),
            },
            Instant::now(),
        );
        assert_eq!(app.ephemeral_status_line(), None);
    }
}
