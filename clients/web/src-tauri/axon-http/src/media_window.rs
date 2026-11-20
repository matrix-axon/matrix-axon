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
//!   asks. At most [`MAX_BLOCKS`] blocks are held, and a block unused for
//!   [`IDLE`] is dropped, as is everything when the page reloads.
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

/// A block nothing has read from for this long is dropped.
pub(crate) const IDLE: Duration = Duration::from_secs(60);

/// The start of the block that holds byte `at`.
pub(crate) fn block_start(at: u64) -> u64 {
    at - at % MAX_BODY as u64
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

#[derive(Default)]
pub struct MediaWindows {
    blocks: Mutex<Vec<Arc<Block>>>,
}

impl MediaWindows {
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

    /// The live block of `ticket` starting at `start`, or else a new one in
    /// its place, with whether it is new and so the caller's to fetch. One
    /// lock covers both, so requests that arrive together share one block.
    fn claim(&self, ticket: &str, start: u64) -> (Arc<Block>, bool) {
        let mut blocks = lock(&self.blocks);
        if let Some(block) = blocks
            .iter()
            .find(|block| block.ticket == ticket && block.first == start && block.live())
        {
            return (Arc::clone(block), false);
        }
        let block = Arc::new(Block::new(ticket, start));
        make_room(&mut blocks);
        blocks.push(Arc::clone(&block));
        (block, true)
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
    pub(crate) async fn serve(
        &self,
        transport: &Transport,
        url: Url,
        ticket: &str,
        spec: Spec,
    ) -> Outcome {
        let Some(first) = self.first_of(ticket, spec) else {
            return Outcome::Bypass;
        };
        let (block, new) = self.claim(ticket, block_start(first));
        if new {
            if let Err(response) = self.fetch(transport, url, &block).await {
                return Outcome::Served(response);
            }
        }
        self.read(&block, spec, first).await
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
        let next = start + MAX_BODY as u64;
        if total.is_some_and(|total| next >= total) {
            return;
        }
        let (block, new) = self.claim(ticket, next);
        if new {
            // A refusal reaches the player when it asks for these bytes
            // itself.
            let _ = self.fetch(transport, url, &block).await;
        }
    }

    /// Fetch `block`, just claimed, returning once its headers are in. A
    /// response that is not a usable `206` is answered as the server sent it
    /// and the block is dropped, which is how a `404` for an expired ticket
    /// reaches the player.
    async fn fetch(
        &self,
        transport: &Transport,
        url: Url,
        block: &Arc<Block>,
    ) -> Result<(), Response<Vec<u8>>> {
        // If this future is dropped before the headers are in, the requests
        // waiting on them go upstream themselves rather than wait for good.
        let mut unsettled = Unsettled(Some((self, block)));
        let first = block.first;
        let range = format!("bytes={first}-{}", first + MAX_BODY as u64 - 1);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::RANGE,
            HeaderValue::from_str(&range).expect("digits and ASCII"),
        );
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        let sent = transport
            .send_once(Outgoing {
                method: Method::GET,
                url,
                headers,
                body: None,
            })
            .await;
        let Ok(response) = sent else {
            return Err(media::bad_gateway());
        };
        let range = (response.status() == StatusCode::PARTIAL_CONTENT)
            .then(|| response.headers().get(header::CONTENT_RANGE))
            .flatten()
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range)
            .filter(|&(start, _, _)| start == first);
        let Some((_, last, total)) = range else {
            return Err(media::shape(&Method::GET, true, response).await);
        };
        let headers = media::forwarded_response_headers(response.headers());
        block.update(|state| {
            state.phase = Phase::Body;
            state.last = last;
            state.total = total;
            state.headers = headers;
            state.bytes.reserve_exact((last - first + 1) as usize);
        });
        unsettled.0 = None;
        tokio::spawn(fill(Arc::clone(block), response));
        Ok(())
    }

    /// Wait for the bytes `spec` needs from `block`, then answer with them.
    async fn read(&self, block: &Arc<Block>, spec: Spec, first: u64) -> Outcome {
        // An open range takes the rest of the block, but returns early on a
        // slow link, as a direct response does.
        let open = !matches!(spec, Spec::Closed(..));
        let early_at = Instant::now() + EARLY_AFTER;
        loop {
            let changed = block.changed.notified();
            match self.decide(block, spec, first, open, early_at) {
                Some(outcome) => return outcome,
                // Wake at the early deadline too, in case nothing more comes.
                // Past it, the next chunk is what can change the answer.
                None if open && Instant::now() < early_at => {
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
        open: bool,
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
        let wanted = match spec {
            Spec::Closed(_, last) => last.min(state.last),
            _ => state.last,
        }
        .min(first + MAX_BODY as u64 - 1);
        let arrived = block.first + state.bytes.len() as u64;
        if arrived > wanted {
            return Some(Outcome::Served(answer(&state, block.first, first, wanted)));
        }
        if state.phase != Phase::Body {
            // Failed, or ended, before the bytes this request needs.
            return Some(Outcome::Served(media::bad_gateway()));
        }
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

/// Before a new block joins `blocks`, drop the idle ones and then, while at
/// the limit, the least recently used.
fn make_room(blocks: &mut Vec<Arc<Block>>) {
    let now = Instant::now();
    blocks.retain(|held| {
        let state = lock(&held.state);
        let keep = matches!(state.phase, Phase::Head | Phase::Body | Phase::Done)
            && now.duration_since(state.used) < IDLE;
        if !keep {
            held.cancel.cancel();
        }
        keep
    });
    while blocks.len() >= MAX_BLOCKS {
        let Some(oldest) = blocks
            .iter()
            .enumerate()
            .min_by_key(|(_, held)| lock(&held.state).used)
            .map(|(i, _)| i)
        else {
            break;
        };
        blocks.remove(oldest).cancel.cancel();
    }
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
/// bytes arrive. Ends when the block is complete, fails, or is dropped.
async fn fill(block: Arc<Block>, mut response: reqwest::Response) {
    let want = {
        let state = lock(&block.state);
        (state.last - block.first + 1) as usize
    };
    let deadline = Instant::now() + media::DEADLINE;
    loop {
        let chunk = tokio::select! {
            biased;
            () = block.cancel.cancelled() => return,
            chunk = tokio::time::timeout_at(deadline, response.chunk()) => chunk,
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
            // Ended short, failed, or ran out of time: the bytes so far
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
