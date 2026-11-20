//! The read-ahead window in front of the `axon-media` scheme.
//!
//! Through WebKit's custom-scheme path, AVFoundation (the macOS and iOS
//! shells) asks for every audio and video sample by itself: about 60
//! requests a second of a few KiB each, and never an open-ended range. Sent
//! upstream one by one, each would cost the server a ticket, token and event
//! check. So a range that no block covers fetches a block of up to
//! [`MAX_BODY`] from its first byte, and later ranges inside that block are
//! answered from memory.
//!
//! - A request waits only for its own bytes, not the whole block, while the
//!   block goes on filling in the background. Requests that arrive while a
//!   block is being fetched share that fetch.
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
    /// Whether a request whose range starts at `at` belongs to this block.
    /// Before the headers arrive the block's end is not known, so any start
    /// within a full block's reach joins it, and is checked again once the
    /// end is.
    fn covers(&self, at: u64) -> bool {
        let state = lock(&self.state);
        match state.phase {
            Phase::Failed | Phase::Refused => false,
            Phase::Head => at >= self.first && at < self.first + MAX_BODY as u64,
            Phase::Body | Phase::Done => at >= self.first && at <= state.last,
        }
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

    /// The block for `ticket` that covers `at`, preferring the latest start.
    fn find(&self, ticket: &str, at: u64) -> Option<Arc<Block>> {
        lock(&self.blocks)
            .iter()
            .filter(|block| block.ticket == ticket && block.covers(at))
            .max_by_key(|block| block.first)
            .cloned()
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

    /// Add `block`, first dropping idle ones and then, while over the limit,
    /// the least recently used.
    fn insert(&self, block: Arc<Block>) {
        let now = Instant::now();
        let mut blocks = lock(&self.blocks);
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
        blocks.push(block);
    }

    /// Answer a `GET` for `spec` of `ticket` (at `url`) from a block,
    /// fetching one first if none covers it.
    pub(crate) async fn serve(
        &self,
        transport: &Transport,
        url: Url,
        ticket: &str,
        spec: Spec,
    ) -> Outcome {
        let first = match spec {
            Spec::Closed(first, _) | Spec::From(first) => first,
            Spec::Suffix(n) => match self.total(ticket) {
                Some(total) => total.saturating_sub(n),
                // Where it starts is the server's to say.
                None => return Outcome::Bypass,
            },
        };
        let block = match self.find(ticket, first) {
            Some(block) => block,
            None => match self.fetch(transport, url, ticket, first).await {
                Ok(block) => block,
                Err(response) => return Outcome::Served(response),
            },
        };
        self.read(&block, spec, first).await
    }

    /// Fetch a block starting at `first`, and return it once its headers are
    /// in. A response that is not a usable `206` is answered as the server
    /// sent it and never held, which is how a `404` for an expired ticket or
    /// a `416` past the end reaches the player.
    async fn fetch(
        &self,
        transport: &Transport,
        url: Url,
        ticket: &str,
        first: u64,
    ) -> Result<Arc<Block>, Response<Vec<u8>>> {
        let block = Arc::new(Block {
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
        });
        self.insert(Arc::clone(&block));

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
        let fail = |block: &Arc<Block>| {
            block.update(|state| state.phase = Phase::Refused);
            self.remove(block);
        };
        let Ok(response) = sent else {
            fail(&block);
            return Err(media::bad_gateway());
        };
        let range = (response.status() == StatusCode::PARTIAL_CONTENT)
            .then(|| response.headers().get(header::CONTENT_RANGE))
            .flatten()
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range)
            .filter(|&(start, _, _)| start == first);
        let Some((_, last, total)) = range else {
            fail(&block);
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
        tokio::spawn(fill(Arc::clone(&block), response));
        Ok(block)
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
