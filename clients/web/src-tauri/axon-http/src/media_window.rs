//! The read-ahead window in front of the `axon-media` scheme.
//!
//! Through WebKit's custom-scheme path, AVFoundation (the macOS and iOS
//! shells) asks for every audio and video sample by itself: about 60
//! requests a second of a few KiB each, and never an open-ended range. Sent
//! upstream one by one, each would cost the server a ticket, token and event
//! check. So the object is read in blocks of [`MAX_BODY`], and ranges inside a
//! block are answered from memory.
//!
//! - Blocks sit on a grid of [`MAX_BODY`] ([`block_start`]). AVFoundation's
//!   audio and video reads often start a few KiB apart, on either side of
//!   wherever a block would otherwise begin; on the grid they share one
//!   block, where blocks starting at each range fetched the same bytes two or
//!   three times over, all at once on a slow link.
//! - A request waits only for its own bytes, not the whole block, while the
//!   block goes on filling in the background. Requests that arrive while a
//!   block is being fetched share that fetch. A range that runs past its
//!   block's end gets the block's part, as a shorter `206`.
//! - Once a read is served from a complete block, the next block is fetched
//!   ([`MediaWindows::read_ahead`]), so a player reading front to back finds
//!   its next bytes already arriving rather than waiting a round trip and a
//!   block for them. That keeps at most one block ahead of what the player has
//!   read, and nothing is fetched while it reads nothing.
//! - Plaintext stays in memory, never on disk, as the server's `no-store`
//!   asks. At most [`MAX_BLOCKS`] blocks are held, and a complete block
//!   unused for [`IDLE`] is dropped, as is everything when the page reloads.
//!   To make room, complete blocks go first, then blocks still arriving that
//!   no request waits on, such as the one a seek moved away from. A block a
//!   request waits on is never dropped: when every block held has one, a
//!   request for another goes upstream as it is.
//! - A block whose upstream sends nothing for [`STALL`] fails, however long
//!   its whole fetch has taken, so a slow but steady link still fills it.
//! - A block outlives its ticket's revocation by at most its own bytes, the
//!   same bound ADR 0139 § 7 accepts for a response already in progress. The
//!   page's stall probe never reads from a block, so an expired ticket is
//!   still seen.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use tauri::http::Response;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::media::{self, parse_content_range, EARLY_AFTER, EARLY_MIN, MAX_BODY};
use crate::transport::{Outgoing, Transport};

/// The most blocks held at once, across every ticket: 32 MiB at most.
pub(crate) const MAX_BLOCKS: usize = 8;

/// A complete block nothing has read from for this long is dropped.
pub(crate) const IDLE: Duration = Duration::from_secs(60);

/// A block's fetch that receives nothing for this long fails.
pub(crate) const STALL: Duration = Duration::from_secs(30);

/// The start of the block that holds byte `at`.
pub(crate) fn block_start(at: u64) -> u64 {
    at - at % MAX_BODY as u64
}

/// The last byte of a block of up to [`MAX_BODY`] that starts at `first`.
fn block_end(first: u64) -> u64 {
    first.saturating_add(MAX_BODY as u64 - 1)
}

/// A `Range` request header the window can answer: one range, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spec {
    /// `bytes=a-b`.
    Closed(u64, u64),
    /// `bytes=a-`.
    From(u64),
    /// `bytes=-n`.
    Suffix(u64),
}

impl Spec {
    /// `None` for anything else, a multi-range request among them, which
    /// then goes upstream as it is.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let spec = value.trim().strip_prefix("bytes=")?;
        let (first, last) = spec.split_once('-')?;
        let (first, last) = (first.trim(), last.trim());
        let number = |s: &str| -> Option<u64> {
            (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .then(|| s.parse().ok())
                .flatten()
        };
        match (first.is_empty(), last.is_empty()) {
            (true, false) => number(last).filter(|&n| n > 0).map(Self::Suffix),
            (false, true) => number(first).map(Self::From),
            (false, false) => {
                let (a, b) = (number(first)?, number(last)?);
                (a <= b).then_some(Self::Closed(a, b))
            }
            (true, true) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for the upstream response's headers.
    Head,
    /// Headers in, body arriving.
    Body,
    /// Every byte the block meant to hold has arrived.
    Done,
    /// The body failed or ended early. The bytes that arrived still answer
    /// the requests waiting on them.
    Failed,
    /// The fetch got no usable `206`. Requests waiting on its headers go
    /// upstream themselves, to get the server's own answer.
    Refused,
}

struct State {
    phase: Phase,
    /// The last byte this block holds once it is complete, from the
    /// upstream `Content-Range`.
    last: u64,
    total: Option<u64>,
    headers: HeaderMap,
    bytes: Vec<u8>,
    used: Instant,
}

struct Block {
    ticket: String,
    first: u64,
    state: Mutex<State>,
    changed: Notify,
    /// Stops the background read when the block is dropped.
    cancel: CancellationToken,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Block {
    fn new(ticket: &str, first: u64) -> Self {
        Self {
            ticket: ticket.to_owned(),
            first,
            state: Mutex::new(State {
                phase: Phase::Head,
                last: 0,
                total: None,
                headers: HeaderMap::new(),
                bytes: Vec::new(),
                used: Instant::now(),
            }),
            changed: Notify::new(),
            cancel: CancellationToken::new(),
        }
    }

    /// Whether the block has, or is still fetching, bytes to serve. A block
    /// that failed or was refused is never joined.
    fn live(&self) -> bool {
        matches!(
            lock(&self.state).phase,
            Phase::Head | Phase::Body | Phase::Done
        )
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut lock(&self.state));
        self.changed.notify_waiters();
    }
}

/// What a lookup found for a request.
pub(crate) enum Outcome {
    /// The window answered.
    Served(Response<Vec<u8>>),
    /// The window cannot answer this one, and it goes upstream as it is.
    Bypass,
}

pub struct MediaWindows {
    blocks: Mutex<Vec<Arc<Block>>>,
    /// [`STALL`], but shorter in tests.
    stall: Duration,
    /// [`IDLE`], but shorter in tests.
    idle: Duration,
}

impl Default for MediaWindows {
    fn default() -> Self {
        Self {
            blocks: Mutex::default(),
            stall: STALL,
            idle: IDLE,
        }
    }
}

impl MediaWindows {
    /// A window whose fetches fail after `stall` without a byte.
    #[cfg(test)]
    pub(crate) fn with_stall(stall: Duration) -> Self {
        Self {
            stall,
            ..Self::default()
        }
    }

    /// A window that drops a complete block unused for `idle`.
    #[cfg(test)]
    pub(crate) fn with_idle(idle: Duration) -> Self {
        Self {
            idle,
            ..Self::default()
        }
    }

    /// Drop every block, as when the page that loaded them goes away.
    pub(crate) fn clear(&self) {
        for block in lock(&self.blocks).drain(..) {
            block.cancel.cancel();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        lock(&self.blocks).len()
    }

    /// The bytes held across every block.
    #[cfg(test)]
    pub(crate) fn held_bytes(&self) -> usize {
        lock(&self.blocks)
            .iter()
            .map(|block| lock(&block.state).bytes.len())
            .sum()
    }

    /// The live block of `ticket` starting at `start`, or else a new one in
    /// its place, with whether it is new and so the caller's to fetch. One
    /// lock covers both, so requests that arrive together share one block.
    /// `None` when there is no room for a new one.
    ///
    /// Idle and failed blocks are dropped before the lookup, so a complete
    /// block unused for [`IDLE`] is never read again, even by a player that
    /// claims nothing new.
    fn claim(&self, ticket: &str, start: u64) -> Option<(Arc<Block>, bool)> {
        let mut blocks = lock(&self.blocks);
        prune(&mut blocks, self.idle);
        if let Some(block) = blocks
            .iter()
            .find(|block| block.ticket == ticket && block.first == start && block.live())
        {
            return Some((Arc::clone(block), false));
        }
        if !make_room(&mut blocks) {
            return None;
        }
        let block = Arc::new(Block::new(ticket, start));
        blocks.push(Arc::clone(&block));
        Some((block, true))
    }

    /// The first byte `spec` asks for, if it can be known here.
    fn first_of(&self, ticket: &str, spec: Spec) -> Option<u64> {
        match spec {
            Spec::Closed(first, _) | Spec::From(first) => Some(first),
            // Where it starts is the server's to say until a block has
            // learned the length.
            Spec::Suffix(n) => self.total(ticket).map(|total| total.saturating_sub(n)),
        }
    }

    /// The object's length, if any block for `ticket` has learned it.
    fn total(&self, ticket: &str) -> Option<u64> {
        lock(&self.blocks)
            .iter()
            .filter(|block| block.ticket == ticket)
            .find_map(|block| lock(&block.state).total)
    }

    fn remove(&self, block: &Arc<Block>) {
        lock(&self.blocks).retain(|held| !Arc::ptr_eq(held, block));
        block.cancel.cancel();
    }

    /// Answer a `GET` for `spec` of `ticket` (at `url`) from the block that
    /// holds its first byte, fetching that block first if it is not held.
    /// With `if_range`, only while it is the block's strong `ETag`: otherwise
    /// the server decides between a `206` and the whole object.
    pub(crate) async fn serve(
        &self,
        transport: &Transport,
        url: Url,
        ticket: &str,
        spec: Spec,
        if_range: Option<&HeaderValue>,
    ) -> Outcome {
        let Some(first) = self.first_of(ticket, spec) else {
            return Outcome::Bypass;
        };
        // Every block held is still arriving: no room for this one.
        let Some((block, new)) = self.claim(ticket, block_start(first)) else {
            return Outcome::Bypass;
        };
        if new {
            match self.fetch(transport, url, &block).await {
                Ok(()) => {}
                Err(Some(response)) => return Outcome::Served(response),
                // Dropped from the window first: go upstream as sent.
                Err(None) => return Outcome::Bypass,
            }
        }
        self.read(&block, spec, first, if_range).await
    }

    /// After `serve` has answered `spec` of `ticket`, fetch the next block if
    /// the one it read from is complete and the next is neither held nor past
    /// the object's end. Returns once that fetch's headers are in, if there
    /// is one.
    pub(crate) async fn read_ahead(
        &self,
        transport: &Transport,
        url: Url,
        ticket: &str,
        spec: Spec,
    ) {
        let Some(first) = self.first_of(ticket, spec) else {
            return;
        };
        let start = block_start(first);
        let complete = lock(&self.blocks).iter().find_map(|block| {
            (block.ticket == ticket && block.first == start)
                .then(|| lock(&block.state))
                .filter(|state| state.phase == Phase::Done)
                .map(|state| state.total)
        });
        let Some(total) = complete else {
            return;
        };
        let Some(next) = start.checked_add(MAX_BODY as u64) else {
            return;
        };
        if total.is_some_and(|total| next >= total) {
            return;
        }
        let Some((block, new)) = self.claim(ticket, next) else {
            return;
        };
        if new {
            // A refusal reaches the player when it asks for these bytes
            // itself.
            let _ = self.fetch(transport, url, &block).await;
        }
    }

    /// Fetch `block`, just claimed, returning once its headers are in. A
    /// response that is not a usable `206` is answered as the server sent it
    /// and the block is dropped, which is how a `404` for an expired ticket
    /// reaches the player. `Err(None)` means the block was dropped from the
    /// window before its headers arrived.
    async fn fetch(
        &self,
        transport: &Transport,
        url: Url,
        block: &Arc<Block>,
    ) -> Result<(), Option<Response<Vec<u8>>>> {
        // If this future is dropped before the headers are in, the requests
        // waiting on them go upstream themselves rather than wait for good.
        let mut unsettled = Unsettled(Some((self, block)));
        let first = block.first;
        let end = block_end(first);
        let range = format!("bytes={first}-{end}");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RANGE,
            HeaderValue::from_str(&range).expect("digits and ASCII"),
        );
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        let send = transport.send_once(Outgoing {
            method: Method::GET,
            url,
            headers,
            body: None,
        });
        // A block dropped while its headers are awaited stops waiting too.
        let sent = tokio::select! {
            biased;
            () = block.cancel.cancelled() => return Err(None),
            sent = send => sent,
        };
        let Ok(response) = sent else {
            return Err(Some(media::bad_gateway()));
        };
        let range = (response.status() == StatusCode::PARTIAL_CONTENT)
            .then(|| response.headers().get(header::CONTENT_RANGE))
            .flatten()
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range)
            .filter(|&(start, _, _)| start == first);
        let Some((_, last, total)) = range else {
            return Err(Some(media::shape(&Method::GET, true, response).await));
        };
        // The span is the peer's to state. A server that sends past the end
        // asked for is read only that far, so a block never holds more than
        // `MAX_BODY`.
        let last = last.min(end);
        let headers = media::forwarded_response_headers(response.headers());
        block.update(|state| {
            state.phase = Phase::Body;
            state.last = last;
            state.total = total;
            state.headers = headers;
            state.bytes.reserve_exact((last - first) as usize + 1);
        });
        unsettled.0 = None;
        tokio::spawn(fill(Arc::clone(block), response, self.stall));
        Ok(())
    }

    /// Wait for the bytes `spec` needs from `block`, then answer with them.
    async fn read(
        &self,
        block: &Arc<Block>,
        spec: Spec,
        first: u64,
        if_range: Option<&HeaderValue>,
    ) -> Outcome {
        let early_at = Instant::now() + EARLY_AFTER;
        loop {
            let changed = block.changed.notified();
            match self.decide(block, spec, first, if_range, early_at) {
                Some(outcome) => return outcome,
                // Wake at the early deadline too, in case nothing more comes.
                // Past it, the next chunk is what can change the answer.
                None if Instant::now() < early_at => {
                    let _ = tokio::time::timeout_at(early_at, changed).await;
                }
                None => changed.await,
            }
        }
    }

    /// What `read` does with the block as it stands: answer, or (`None`)
    /// wait for it to change.
    fn decide(
        &self,
        block: &Block,
        spec: Spec,
        first: u64,
        if_range: Option<&HeaderValue>,
        early_at: Instant,
    ) -> Option<Outcome> {
        let mut state = lock(&block.state);
        state.used = Instant::now();
        if state.phase == Phase::Head {
            return None;
        }
        if state.phase == Phase::Refused || first > state.last {
            // No block after all; or joined before the end was known, and
            // past it.
            return Some(Outcome::Bypass);
        }
        if if_range.is_some_and(|validator| !strong_match(validator, &state.headers)) {
            return Some(Outcome::Bypass);
        }
        let wanted = match spec {
            Spec::Closed(_, last) => last.min(state.last),
            _ => state.last,
        };
        let arrived = block.first + state.bytes.len() as u64;
        if arrived > wanted {
            return Some(Outcome::Served(answer(&state, block.first, first, wanted)));
        }
        if state.phase != Phase::Body {
            // Failed, or ended, before the bytes this request needs.
            return Some(Outcome::Served(media::bad_gateway()));
        }
        // A range that takes the rest of the block, open or closed at or past
        // its end (AVFoundation's `bytes=0-<length - 1>` over http), returns
        // early on a slow link, as a direct response does.
        let open = !matches!(spec, Spec::Closed(_, last) if last < state.last);
        let have = arrived.saturating_sub(first) as usize;
        if open && have >= EARLY_MIN && Instant::now() >= early_at {
            return Some(Outcome::Served(answer(
                &state,
                block.first,
                first,
                arrived - 1,
            )));
        }
        None
    }
}

/// Whether `validator`, an `If-Range`, is the block's `ETag` and both are
/// strong (RFC 9110 § 13.1.5). A date, a weak tag or a block without one
/// never matches.
fn strong_match(validator: &HeaderValue, headers: &HeaderMap) -> bool {
    headers
        .get(header::ETAG)
        .is_some_and(|etag| etag == validator && !etag.as_bytes().starts_with(b"W/"))
}

/// Drop the failed blocks and the complete ones unused for `idle`.
fn prune(blocks: &mut Vec<Arc<Block>>, idle: Duration) {
    let now = Instant::now();
    blocks.retain(|held| {
        let state = lock(&held.state);
        // `used` can be stamped after `now` was read, hence saturating.
        let keep = match state.phase {
            Phase::Head | Phase::Body => true,
            Phase::Done => now.saturating_duration_since(state.used) < idle,
            Phase::Failed | Phase::Refused => false,
        };
        if !keep {
            held.cancel.cancel();
        }
        keep
    });
}

/// Before a new block joins `blocks`, already [`prune`]d, drop while at the
/// limit the least recently used complete block, or else the least recently
/// used arriving one that no request waits on. Whether that left room for it.
///
/// A block a request waits on is never dropped here: the request does not
/// touch `used` until bytes arrive, and would be failed for nothing. `fill`
/// ends a fetch that stops sending ([`STALL`]).
///
/// Every request holds its own `Arc` of the block it reads, and gets it only
/// from `claim`, under the lock the caller holds. So a block in `Body` held by
/// nothing but this list and its `fill` task has no request waiting on it,
/// and none can start to wait until the lock is released. A block in `Head`
/// always has its fetcher waiting on it.
fn make_room(blocks: &mut Vec<Arc<Block>>) -> bool {
    while blocks.len() >= MAX_BLOCKS {
        let Some(oldest) = blocks
            .iter()
            .enumerate()
            .filter_map(|(i, held)| {
                let state = lock(&held.state);
                let unread = state.phase == Phase::Body && Arc::strong_count(held) == 2;
                // Complete blocks sort first.
                match state.phase {
                    Phase::Done => Some(((false, state.used), i)),
                    _ if unread => Some(((true, state.used), i)),
                    _ => None,
                }
            })
            .min()
            .map(|(_, i)| i)
        else {
            return false;
        };
        blocks.remove(oldest).cancel.cancel();
    }
    true
}

/// A block whose fetch has not yet got a usable `206`: refused and dropped
/// when this is, unless the fetch cleared it first.
struct Unsettled<'a>(Option<(&'a MediaWindows, &'a Arc<Block>)>);

impl Drop for Unsettled<'_> {
    fn drop(&mut self) {
        if let Some((windows, block)) = self.0.take() {
            block.update(|state| state.phase = Phase::Refused);
            windows.remove(block);
        }
    }
}

/// A `206` with bytes `from..=to` of the object, out of a block whose first
/// byte is `start`.
fn answer(state: &State, start: u64, from: u64, to: u64) -> Response<Vec<u8>> {
    let body = state.bytes[(from - start) as usize..=(to - start) as usize].to_vec();
    let total = state
        .total
        .map_or_else(|| "*".to_owned(), |total| total.to_string());
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    let headers = response.headers_mut();
    headers.extend(state.headers.clone());
    headers.insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {from}-{to}/{total}")).expect("digits and ASCII"),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(to - from + 1));
    response
}

/// Read the rest of a block's body in the background, waking readers as
/// bytes arrive. Ends when the block is complete, fails, is dropped, or gets
/// nothing for `stall`.
async fn fill(block: Arc<Block>, mut response: reqwest::Response, stall: Duration) {
    let want = {
        let state = lock(&block.state);
        (state.last - block.first + 1) as usize
    };
    loop {
        let chunk = tokio::select! {
            biased;
            () = block.cancel.cancelled() => {
                // Dropped from the window: the requests waiting on it get
                // what has arrived, or a `502`, rather than wait for good.
                block.update(|state| {
                    if state.phase == Phase::Body {
                        state.phase = Phase::Failed;
                    }
                });
                return;
            }
            chunk = tokio::time::timeout(stall, response.chunk()) => chunk,
        };
        let mut done = false;
        block.update(|state| match chunk {
            Ok(Ok(Some(bytes))) => {
                let take = bytes.len().min(want - state.bytes.len());
                state.bytes.extend_from_slice(&bytes[..take]);
                if state.bytes.len() == want {
                    state.phase = Phase::Done;
                    done = true;
                }
            }
            // Ended short, failed, or stalled: the bytes so far
            // still serve the requests they cover.
            _ => {
                state.phase = Phase::Failed;
                done = true;
            }
        });
        if done {
            return;
        }
    }
}
