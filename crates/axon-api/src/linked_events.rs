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
use std::sync::atomic::{AtomicU64, Ordering};
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
/// The most one kept entry may take, serialized. A projection over it is not
/// kept, so an account's entries take at most about 1 MiB.
pub const KEPT_ENTRY_BYTES: usize = 4 * 1024;
/// How often expired entries are reclaimed across every account, so an idle
/// or deleted account's entries do not wait for its next read.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The media fields of fetched events, per account, least recently used
/// first out. In memory: a restart forgets them, and the card reads its event
/// again.
///
/// Every read of an event takes a [`ticket`](Self::ticket) before it asks the
/// homeserver, and its outcome, kept media or a withdrawal, applies only if no
/// read that started later has already been applied. So a slow, older answer
/// cannot restore media a newer read found gone.
pub struct KeptMedia {
    state: Mutex<KeptState>,
    tickets: AtomicU64,
}

struct KeptState {
    accounts: HashMap<Uuid, AccountKept>,
    swept_at: Instant,
}

impl Default for KeptMedia {
    fn default() -> Self {
        Self {
            state: Mutex::new(KeptState {
                accounts: HashMap::new(),
                swept_at: Instant::now(),
            }),
            tickets: AtomicU64::new(0),
        }
    }
}

#[derive(Default)]
struct AccountKept {
    entries: HashMap<(String, String), Kept>,
    /// Events whose latest applied read found no media, or no event, so an
    /// older read still in flight cannot restore media over that answer. Kept
    /// apart from `entries` and capped on their own, so refusals (a link to a
    /// room the account was never in) never push out media a card is showing.
    withdrawn: HashMap<(String, String), Withdrawn>,
    /// Bumped on every keep and use, to order entries by recency.
    clock: u64,
}

struct Kept {
    media: Value,
    ticket: u64,
    kept_at: Instant,
    used: u64,
}

struct Withdrawn {
    ticket: u64,
    at: Instant,
}

impl AccountKept {
    /// The ticket of the latest read applied for `key`, kept or withdrawn.
    fn applied_ticket(&self, key: &(String, String)) -> Option<u64> {
        self.entries
            .get(key)
            .map(|kept| kept.ticket)
            .or_else(|| self.withdrawn.get(key).map(|gone| gone.ticket))
    }

    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|_, kept| now.duration_since(kept.kept_at) < KEPT_FOR);
        self.withdrawn
            .retain(|_, gone| now.duration_since(gone.at) < KEPT_FOR);
    }
}

impl KeptMedia {
    /// A ticket for one read, taken before it asks the homeserver.
    pub fn ticket(&self) -> u64 {
        self.tickets.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Keep the media fields of a fetched event's content. Content that names
    /// no media, or whose projection is too large, keeps nothing and
    /// withdraws what an earlier read kept.
    pub fn keep(
        &self,
        ticket: u64,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        content: &Value,
    ) {
        let media = media_fields(content);
        self.apply_at(Instant::now(), ticket, account_id, room_id, event_id, media);
    }

    /// Withdraw what was kept for an event: the latest read found it redacted,
    /// or not there to show.
    pub fn forget(&self, ticket: u64, account_id: Uuid, room_id: &str, event_id: &str) {
        self.apply_at(Instant::now(), ticket, account_id, room_id, event_id, None);
    }

    /// The kept media fields of an event, if they are still kept.
    pub fn get(&self, account_id: Uuid, room_id: &str, event_id: &str) -> Option<Value> {
        self.get_at(Instant::now(), account_id, room_id, event_id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, KeptState> {
        self.state.lock().expect("kept media poisoned")
    }

    fn apply_at(
        &self,
        now: Instant,
        ticket: u64,
        account_id: Uuid,
        room_id: &str,
        event_id: &str,
        media: Option<Value>,
    ) {
        let key = (room_id.to_owned(), event_id.to_owned());
        let mut state = self.lock();
        state.sweep(now);
        let account = state.accounts.entry(account_id).or_default();
        if account
            .applied_ticket(&key)
            .is_some_and(|applied| applied > ticket)
        {
            return;
        }
        account.expire(now);
        let Some(media) = media else {
            account.entries.remove(&key);
            if !account.withdrawn.contains_key(&key) && account.withdrawn.len() >= KEPT_PER_ACCOUNT
            {
                // The oldest withdrawal is the one least likely to have an
                // older read still in flight behind it.
                if let Some(oldest) = account
                    .withdrawn
                    .iter()
                    .min_by_key(|(_, gone)| gone.ticket)
                    .map(|(key, _)| key.clone())
                {
                    account.withdrawn.remove(&oldest);
                }
            }
            account.withdrawn.insert(key, Withdrawn { ticket, at: now });
            return;
        };
        account.withdrawn.remove(&key);
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
                ticket,
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
        let mut state = self.lock();
        state.sweep(now);
        let account = state.accounts.get_mut(&account_id)?;
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

    #[cfg(test)]
    fn entries(&self, account_id: Uuid) -> usize {
        self.lock()
            .accounts
            .get(&account_id)
            .map_or(0, |account| account.entries.len())
    }

    #[cfg(test)]
    fn withdrawn(&self, account_id: Uuid) -> usize {
        self.lock()
            .accounts
            .get(&account_id)
            .map_or(0, |account| account.withdrawn.len())
    }
}

impl KeptState {
    /// Reclaim expired entries in every account, and accounts left empty, at
    /// most once per [`SWEEP_EVERY`].
    fn sweep(&mut self, now: Instant) {
        if now.duration_since(self.swept_at) < SWEEP_EVERY {
            return;
        }
        self.swept_at = now;
        self.accounts.retain(|_, account| {
            account.expire(now);
            !account.entries.is_empty() || !account.withdrawn.is_empty()
        });
    }
}

/// The keys of the encrypted-file object the SDK writes; anything else in a
/// descriptor is dropped.
const ENCRYPTED_FILE_KEYS: &[&str] = &["url", "key", "iv", "hashes", "v"];
/// The longest MIME type kept.
const MAX_MIMETYPE_BYTES: usize = 255;

/// The fields the media route resolves a card's image with (section 4): the
/// URIs and encrypted-file descriptors in `url`, `file`, `info.thumbnail_url`
/// and `info.thumbnail_file`, and their MIME types. Only well-formed values are
/// kept: an `mxc://` URI of bounded length, a descriptor of the SDK's keys
/// whose URI is one, a short string MIME type. `None` when the content names
/// no usable media, or the projection exceeds [`KEPT_ENTRY_BYTES`].
pub(crate) fn media_fields(content: &Value) -> Option<Value> {
    let source_info = content.get("info").unwrap_or(&Value::Null);
    let mut media = Map::new();
    let mut info = Map::new();
    if let Some(url) = mxc(content.get("url")) {
        media.insert("url".to_owned(), url);
    }
    if let Some(file) = encrypted_file(content.get("file")) {
        media.insert("file".to_owned(), file);
    }
    if let Some(url) = mxc(source_info.get("thumbnail_url")) {
        info.insert("thumbnail_url".to_owned(), url);
    }
    if let Some(file) = encrypted_file(source_info.get("thumbnail_file")) {
        info.insert("thumbnail_file".to_owned(), file);
    }
    if media.is_empty() && info.is_empty() {
        return None;
    }
    if let Some(mimetype) = mimetype(source_info.get("mimetype")) {
        info.insert("mimetype".to_owned(), mimetype);
    }
    if let Some(mimetype) = mimetype(
        source_info
            .get("thumbnail_info")
            .and_then(|ti| ti.get("mimetype")),
    ) {
        info.insert(
            "thumbnail_info".to_owned(),
            serde_json::json!({ "mimetype": mimetype }),
        );
    }
    if !info.is_empty() {
        media.insert("info".to_owned(), Value::Object(info));
    }
    let media = Value::Object(media);
    let bytes = serde_json::to_vec(&media).map_or(usize::MAX, |b| b.len());
    (bytes <= KEPT_ENTRY_BYTES).then_some(media)
}

fn mxc(value: Option<&Value>) -> Option<Value> {
    let url = value?.as_str()?;
    (url.len() <= axon_core::link_preview::MAX_MXC_LENGTH
        && axon_core::media::parse_mxc(url).is_some())
    .then(|| Value::String(url.to_owned()))
}

fn encrypted_file(value: Option<&Value>) -> Option<Value> {
    let object = value?.as_object()?;
    mxc(object.get("url"))?;
    let kept: Map<String, Value> = object
        .iter()
        .filter(|(key, _)| ENCRYPTED_FILE_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let bytes = serde_json::to_vec(&kept).map_or(usize::MAX, |b| b.len());
    (bytes <= axon_core::link_preview::MAX_MEDIA_SOURCE_BYTES).then_some(Value::Object(kept))
}

fn mimetype(value: Option<&Value>) -> Option<Value> {
    let mimetype = value?.as_str()?;
    (mimetype.len() <= MAX_MIMETYPE_BYTES).then(|| Value::String(mimetype.to_owned()))
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

    fn descriptor(url: &str) -> Value {
        json!({
            "url": url,
            "key": {"kty": "oct", "key_ops": ["encrypt", "decrypt"], "alg": "A256CTR", "k": "abc", "ext": true},
            "iv": "AAAA",
            "hashes": {"sha256": "hash"},
            "v": "v2",
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
    fn an_encrypted_file_keeps_only_its_descriptor_keys() {
        let mut file = descriptor("mxc://example.org/enc");
        file["padding"] = json!("x".repeat(4096));
        let media = media_fields(
            &json!({"msgtype": "m.file", "file": file, "info": {"mimetype": "application/pdf"}}),
        )
        .unwrap();
        assert_eq!(media["file"], descriptor("mxc://example.org/enc"));
        assert!(names_media(&media, "mxc://example.org/enc"));
        assert!(!names_media(&media, "mxc://example.org/other"));
        assert!(serde_json::to_vec(&media).unwrap().len() < 1024);
    }

    #[test]
    fn malformed_fields_are_not_kept() {
        let long_mxc = format!("mxc://example.org/{}", "a".repeat(2048));
        assert_eq!(media_fields(&json!({"url": long_mxc})), None);
        assert_eq!(
            media_fields(&json!({"url": "https://example.org/a.png"})),
            None
        );
        assert_eq!(media_fields(&json!({"url": {"not": "a string"}})), None);
        assert_eq!(
            media_fields(&json!({"file": {"url": "nope", "key": {}}})),
            None
        );
        let mut big_key = descriptor("mxc://example.org/enc");
        big_key["key"]["k"] = json!("k".repeat(2048));
        assert_eq!(media_fields(&json!({"file": big_key})), None);
        let media = media_fields(&json!({
            "url": "mxc://example.org/a",
            "info": {"mimetype": "x".repeat(1000), "thumbnail_info": {"mimetype": 5}},
        }))
        .unwrap();
        assert_eq!(media, json!({"url": "mxc://example.org/a"}));
    }

    #[test]
    fn kept_media_is_per_account_and_per_event() {
        let kept = KeptMedia::default();
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
        kept.keep(
            kept.ticket(),
            alice,
            "!r",
            "$e",
            &image("mxc://example.org/a"),
        );
        assert!(kept.get(alice, "!r", "$e").is_some());
        assert!(kept.get(bob, "!r", "$e").is_none());
        assert!(kept.get(alice, "!other", "$e").is_none());
        // A later read naming no media withdraws what was kept.
        kept.keep(
            kept.ticket(),
            alice,
            "!r",
            "$e",
            &json!({"msgtype": "m.text", "body": "edited"}),
        );
        assert!(kept.get(alice, "!r", "$e").is_none());
    }

    #[test]
    fn a_refusal_withdraws_and_an_older_answer_cannot_restore() {
        let kept = KeptMedia::default();
        let account = Uuid::new_v4();
        let first = kept.ticket();
        kept.keep(first, account, "!r", "$e", &image("mxc://example.org/a"));
        // Two reads start; the newer is refused first, then the older answers.
        let older = kept.ticket();
        let newer = kept.ticket();
        kept.forget(newer, account, "!r", "$e");
        assert!(kept.get(account, "!r", "$e").is_none());
        kept.keep(older, account, "!r", "$e", &image("mxc://example.org/a"));
        assert!(
            kept.get(account, "!r", "$e").is_none(),
            "the refusal stands"
        );
        // A read that starts after the refusal can keep again.
        kept.keep(
            kept.ticket(),
            account,
            "!r",
            "$e",
            &image("mxc://example.org/a"),
        );
        assert!(kept.get(account, "!r", "$e").is_some());
    }

    #[test]
    fn kept_media_expires_after_an_hour() {
        let kept = KeptMedia::default();
        let account = Uuid::new_v4();
        let start = Instant::now();
        let media = media_fields(&image("mxc://example.org/a"));
        kept.apply_at(start, kept.ticket(), account, "!r", "$e", media);
        let almost = start + KEPT_FOR - Duration::from_secs(1);
        assert!(kept.get_at(almost, account, "!r", "$e").is_some());
        assert!(kept.get_at(start + KEPT_FOR, account, "!r", "$e").is_none());
    }

    #[test]
    fn an_idle_accounts_expired_entries_are_swept() {
        let kept = KeptMedia::default();
        let (idle, busy) = (Uuid::new_v4(), Uuid::new_v4());
        let start = Instant::now();
        let media = media_fields(&image("mxc://example.org/a"));
        kept.apply_at(start, kept.ticket(), idle, "!r", "$e", media.clone());
        assert_eq!(kept.entries(idle), 1);
        // Only another account is used after the hour.
        kept.apply_at(
            start + KEPT_FOR + SWEEP_EVERY,
            kept.ticket(),
            busy,
            "!r",
            "$e",
            media,
        );
        assert_eq!(kept.entries(idle), 0);
        assert!(!kept.lock().accounts.contains_key(&idle));
    }

    #[test]
    fn the_least_recently_used_goes_first_and_only_in_its_own_account() {
        let kept = KeptMedia::default();
        let (busy, quiet) = (Uuid::new_v4(), Uuid::new_v4());
        let now = Instant::now();
        let media = media_fields(&image("mxc://example.org/a"));
        kept.apply_at(now, kept.ticket(), quiet, "!r", "$q", media.clone());
        for n in 0..KEPT_PER_ACCOUNT {
            kept.apply_at(
                now,
                kept.ticket(),
                busy,
                "!r",
                &format!("${n}"),
                media.clone(),
            );
        }
        // Use the oldest, so the second oldest is now least recently used.
        assert!(kept.get_at(now, busy, "!r", "$0").is_some());
        kept.apply_at(now, kept.ticket(), busy, "!r", "$new", media);
        assert!(kept.get_at(now, busy, "!r", "$0").is_some());
        assert!(kept.get_at(now, busy, "!r", "$1").is_none());
        assert!(kept.get_at(now, busy, "!r", "$new").is_some());
        assert!(kept.get_at(now, quiet, "!r", "$q").is_some());
        assert_eq!(kept.entries(busy), KEPT_PER_ACCOUNT);
    }

    #[test]
    fn refusals_never_push_out_kept_media() {
        let kept = KeptMedia::default();
        let account = Uuid::new_v4();
        let now = Instant::now();
        let media = media_fields(&image("mxc://example.org/a"));
        kept.apply_at(now, kept.ticket(), account, "!r", "$shown", media);
        // Links to rooms the account was never in: every read is refused.
        let mut last = 0;
        for n in 0..KEPT_PER_ACCOUNT * 2 {
            last = kept.ticket();
            kept.apply_at(now, last, account, "!unknown", &format!("${n}"), None);
        }
        assert!(kept.get_at(now, account, "!r", "$shown").is_some());
        assert_eq!(kept.entries(account), 1);
        assert_eq!(kept.withdrawn(account), KEPT_PER_ACCOUNT);
        // The newest withdrawals are the ones kept: an older read of the
        // last refused event still cannot restore its media.
        let newest = format!("${}", KEPT_PER_ACCOUNT * 2 - 1);
        kept.apply_at(
            now,
            last - 1,
            account,
            "!unknown",
            &newest,
            media_fields(&image("mxc://example.org/a")),
        );
        assert!(kept.get_at(now, account, "!unknown", &newest).is_none());
    }
}
