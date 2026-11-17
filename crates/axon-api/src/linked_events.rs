//! Matrix-link cards (ADR 0138 sections 3 and 4): the port the linked-event
//! route reads the homeserver through, and the media details it keeps from
//! what it read.
//!
//! Like [`SenderTrustService`](crate::trust::SenderTrustService), this crate
//! defines the capability it needs. The implementation lives in `axon-sync`
//! (it needs a live client to fetch and decrypt), adapted onto this port by
//! `axon-server`.
//!
//! An event read from the homeserver is never stored, so the media route
//! cannot resolve its image through the store. [`KeptMedia`] holds, for an
//! hour, the media fields of each such event's content, and the media route
//! resolves a card's image against them when the request names the event.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Map, Value};
use uuid::Uuid;

/// One event as the homeserver serves it to this account, its redaction and
/// any bundled edit already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchedEvent {
    pub event_id: String,
    pub room_id: String,
    pub sender: String,
    pub origin_ts: i64,
    pub event_type: String,
    pub content: Option<Value>,
    pub body: Option<String>,
    pub relates_to: Option<Value>,
    pub redacted: bool,
    pub edited: bool,
    pub sender_trust: Option<String>,
}

/// Why a linked event could not be read. Small and HTTP-shaped; the adapter
/// collapses the backend's error into one of these.
#[derive(Debug)]
pub enum LinkedEventError {
    /// Nothing to show, or no such account. → `404`.
    NotFound(String),
    /// A malformed room or event id. → `400`.
    Invalid(String),
    /// The account is logged out or being deleted. → `409`.
    NotActive(String),
    /// Not connected, or no capacity within the permit wait. → `503`.
    Unavailable(String),
    /// The homeserver failed. → `502`.
    Upstream(String),
    /// The homeserver did not answer in time. → `504`.
    Timeout(String),
    /// Logged where it happened; the body is generic. → `500`.
    Internal,
}

/// Reads one event from the account's homeserver. Implemented outside this
/// crate; held in [`AppState`](crate::AppState).
#[async_trait]
pub trait LinkedEventSource: Send + Sync {
    async fn read(
        &self,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Result<FetchedEvent, LinkedEventError>;
}

/// Fallback when the binary injects no source (API tests, configurations
/// without sync): the homeserver is never asked, so only stored events have
/// cards.
pub(crate) struct NoLinkedEventSource;

#[async_trait]
impl LinkedEventSource for NoLinkedEventSource {
    async fn read(&self, _: Uuid, _: &str, _: &str) -> Result<FetchedEvent, LinkedEventError> {
        Err(LinkedEventError::NotFound("event not found".to_owned()))
    }
}

/// Fetched events whose media fields are kept, per account (ADR 0138 section
/// 4). Sized to what the client can have on screen: the visible messages and a
/// screen's height either side, at most three linked events each.
pub const KEPT_PER_ACCOUNT: usize = 256;
/// How long a fetched event's media fields are kept.
pub const KEPT_FOR: Duration = Duration::from_secs(60 * 60);

/// The media fields of fetched events, per account, least recently used
/// first out. In memory: a restart forgets them, and the card reads its event
/// again.
#[derive(Default)]
pub struct KeptMedia {
    accounts: Mutex<HashMap<Uuid, AccountKept>>,
}

#[derive(Default)]
struct AccountKept {
    entries: HashMap<(String, String), Kept>,
    /// Bumped on every keep and use, to order entries by recency.
    clock: u64,
}

struct Kept {
    media: Value,
    kept_at: Instant,
    used: u64,
}

impl KeptMedia {
    /// Keep the media fields of a fetched event's content. An event that names
    /// no media keeps nothing, and drops what an earlier read of it kept.
    pub fn keep(&self, account_id: Uuid, room_id: &str, event_id: &str, content: &Value) {
        self.keep_at(Instant::now(), account_id, room_id, event_id, content);
    }

    /// The kept media fields of an event, if they are still kept.
    pub fn get(&self, account_id: Uuid, room_id: &str, event_id: &str) -> Option<Value> {
        self.get_at(Instant::now(), account_id, room_id, event_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, AccountKept>> {
        self.accounts.lock().expect("kept media poisoned")
    }

    fn keep_at(
        &self,
        now: Instant,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        content: &Value,
    ) {
        let key = (room_id.to_owned(), event_id.to_owned());
        let mut accounts = self.lock();
        let Some(media) = media_fields(content) else {
            if let Some(account) = accounts.get_mut(&account_id) {
                account.entries.remove(&key);
            }
            return;
        };
        let account = accounts.entry(account_id).or_default();
        account
            .entries
            .retain(|_, kept| now.duration_since(kept.kept_at) < KEPT_FOR);
        if !account.entries.contains_key(&key) && account.entries.len() >= KEPT_PER_ACCOUNT {
            if let Some(oldest) = account
                .entries
                .iter()
                .min_by_key(|(_, kept)| kept.used)
                .map(|(key, _)| key.clone())
            {
                account.entries.remove(&oldest);
            }
        }
        account.clock += 1;
        let used = account.clock;
        account.entries.insert(
            key,
            Kept {
                media,
                kept_at: now,
                used,
            },
        );
    }

    fn get_at(
        &self,
        now: Instant,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
    ) -> Option<Value> {
        let mut accounts = self.lock();
        let account = accounts.get_mut(&account_id)?;
        let key = (room_id.to_owned(), event_id.to_owned());
        let expired = now.duration_since(account.entries.get(&key)?.kept_at) >= KEPT_FOR;
        if expired {
            account.entries.remove(&key);
            return None;
        }
        account.clock += 1;
        let used = account.clock;
        let kept = account.entries.get_mut(&key)?;
        kept.used = used;
        Some(kept.media.clone())
    }
}

/// The fields the media route resolves a card's image with (section 4): the
/// URIs and encrypted-file descriptors in `url`, `file`, `info.thumbnail_url`
/// and `info.thumbnail_file`, and their MIME types. `None` when the content
/// names no media.
pub(crate) fn media_fields(content: &Value) -> Option<Value> {
    let mut media = Map::new();
    let mut info = Map::new();
    let copy = |from: &Value, key: &str, into: &mut Map<String, Value>| {
        if let Some(value) = from.get(key).filter(|v| !v.is_null()) {
            into.insert(key.to_owned(), value.clone());
        }
    };
    copy(content, "url", &mut media);
    copy(content, "file", &mut media);
    let source_info = content.get("info").cloned().unwrap_or(Value::Null);
    copy(&source_info, "thumbnail_url", &mut info);
    copy(&source_info, "thumbnail_file", &mut info);
    if media.is_empty() && info.is_empty() {
        return None;
    }
    copy(&source_info, "mimetype", &mut info);
    if let Some(mimetype) = source_info
        .get("thumbnail_info")
        .and_then(|ti| ti.get("mimetype"))
    {
        info.insert(
            "thumbnail_info".to_owned(),
            serde_json::json!({ "mimetype": mimetype }),
        );
    }
    if !info.is_empty() {
        media.insert("info".to_owned(), Value::Object(info));
    }
    Some(Value::Object(media))
}

/// Whether `content` names `mxc_url` as its media or its thumbnail, in `url`,
/// `file.url`, `info.thumbnail_url` or `info.thumbnail_file.url`: the only
/// places a card's media may come from.
pub(crate) fn names_media(content: &Value, mxc_url: &str) -> bool {
    let names = |value: &Value| value.as_str() == Some(mxc_url);
    names(&content["url"])
        || names(&content["file"]["url"])
        || names(&content["info"]["thumbnail_url"])
        || names(&content["info"]["thumbnail_file"]["url"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn image(mxc: &str) -> Value {
        json!({
            "msgtype": "m.image",
            "body": "cat.png",
            "url": mxc,
            "info": {
                "mimetype": "image/png",
                "w": 10,
                "thumbnail_url": "mxc://example.org/thumb",
                "thumbnail_info": {"mimetype": "image/jpeg", "w": 5},
            },
        })
    }

    #[test]
    fn only_media_fields_are_kept() {
        let media = media_fields(&image("mxc://example.org/a")).unwrap();
        assert_eq!(
            media,
            json!({
                "url": "mxc://example.org/a",
                "info": {
                    "thumbnail_url": "mxc://example.org/thumb",
                    "mimetype": "image/png",
                    "thumbnail_info": {"mimetype": "image/jpeg"},
                },
            })
        );
        assert_eq!(
            media_fields(&json!({"msgtype": "m.text", "body": "hi"})),
            None
        );
    }

    #[test]
    fn an_encrypted_file_keeps_its_descriptor() {
        let file = json!({"url": "mxc://example.org/enc", "key": {"k": "x"}, "iv": "i", "hashes": {"sha256": "h"}, "v": "v2"});
        let media = media_fields(&json!({"msgtype": "m.file", "file": file.clone(), "info": {"mimetype": "application/pdf"}})).unwrap();
        assert_eq!(media["file"], file);
        assert!(names_media(&media, "mxc://example.org/enc"));
        assert!(!names_media(&media, "mxc://example.org/other"));
    }

    #[test]
    fn kept_media_is_per_account_and_per_event() {
        let kept = KeptMedia::default();
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
        kept.keep(alice, "!r", "$e", &image("mxc://example.org/a"));
        assert!(kept.get(alice, "!r", "$e").is_some());
        assert!(kept.get(bob, "!r", "$e").is_none());
        assert!(kept.get(alice, "!other", "$e").is_none());
        // A later read naming no media drops what was kept.
        kept.keep(
            alice,
            "!r",
            "$e",
            &json!({"msgtype": "m.text", "body": "edited"}),
        );
        assert!(kept.get(alice, "!r", "$e").is_none());
    }

    #[test]
    fn kept_media_expires_after_an_hour() {
        let kept = KeptMedia::default();
        let account = Uuid::new_v4();
        let start = Instant::now();
        kept.keep_at(start, account, "!r", "$e", &image("mxc://example.org/a"));
        let almost = start + KEPT_FOR - Duration::from_secs(1);
        assert!(kept.get_at(almost, account, "!r", "$e").is_some());
        assert!(kept.get_at(start + KEPT_FOR, account, "!r", "$e").is_none());
    }

    #[test]
    fn the_least_recently_used_goes_first_and_only_in_its_own_account() {
        let kept = KeptMedia::default();
        let (busy, quiet) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Instant::now();
        kept.keep_at(now, quiet, "!r", "$q", &image("mxc://example.org/q"));
        for n in 0..KEPT_PER_ACCOUNT {
            kept.keep_at(
                now,
                busy,
                "!r",
                &format!("${n}"),
                &image("mxc://example.org/a"),
            );
        }
        // Use the oldest, so the second oldest is now least recently used.
        assert!(kept.get_at(now, busy, "!r", "$0").is_some());
        kept.keep_at(now, busy, "!r", "$new", &image("mxc://example.org/a"));
        assert!(kept.get_at(now, busy, "!r", "$0").is_some());
        assert!(kept.get_at(now, busy, "!r", "$1").is_none());
        assert!(kept.get_at(now, busy, "!r", "$new").is_some());
        assert!(kept.get_at(now, quiet, "!r", "$q").is_some());
    }
}
