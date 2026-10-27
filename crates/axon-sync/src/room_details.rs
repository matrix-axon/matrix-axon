//! Coalesced `room_details.changed` frames (ADR 0126 decision 5).
//!
//! The primary sync dispatches a room's state one event at a time, with no
//! hook at the end of the room's batch, so producers mark each real change
//! here and one per-account flusher publishes a frame per room once the
//! account has been quiet for [`QUIET`]. Under continuous traffic it still
//! publishes [`MAX_DELAY`] after the first pending mark, so a busy account
//! cannot hold a frame back indefinitely. A room whose batch straddles a flush
//! gets a second frame; a frame is a signal to re-read, so that costs one
//! extra read and never loses a change.
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axon_core::{LiveFrame, RoomDetailSource, RoomDetailsFrame};
use tokio::sync::{broadcast, Notify};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// How long the account must be quiet before pending frames go out.
const QUIET: Duration = Duration::from_millis(100);
/// The longest a mark waits while other marks keep arriving.
const MAX_DELAY: Duration = Duration::from_secs(1);
/// Pending rooms are published at once when this many are waiting, so a
/// large initial sync cannot grow the map with the account's room count.
const MAX_PENDING_ROOMS: usize = 256;

type Rooms = HashMap<String, BTreeSet<RoomDetailSource>>;

/// One account's coalescer; clones share it.
#[derive(Clone)]
pub(crate) struct RoomDetailsChanges(Arc<Shared>);

struct Shared {
    account_id: Uuid,
    live_tx: broadcast::Sender<LiveFrame>,
    pending: Mutex<Pending>,
    wake: Notify,
}

#[derive(Default)]
struct Pending {
    rooms: Rooms,
    /// Counts marks, so the flusher can tell whether any arrived while it
    /// slept.
    marks: u64,
}

impl std::fmt::Debug for RoomDetailsChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomDetailsChanges")
            .field("account_id", &self.0.account_id)
            .finish_non_exhaustive()
    }
}

impl RoomDetailsChanges {
    pub(crate) fn new(account_id: Uuid, live_tx: broadcast::Sender<LiveFrame>) -> Self {
        Self(Arc::new(Shared {
            account_id,
            live_tx,
            pending: Mutex::new(Pending::default()),
            wake: Notify::new(),
        }))
    }

    /// Record that `source` changed for `room_id`. Call only after a write
    /// reported a real change.
    pub(crate) fn mark(&self, room_id: &str, source: RoomDetailSource) {
        let overflow = {
            let mut pending = self.lock();
            pending.marks = pending.marks.wrapping_add(1);
            pending
                .rooms
                .entry(room_id.to_owned())
                .or_default()
                .insert(source);
            (pending.rooms.len() >= MAX_PENDING_ROOMS).then(|| std::mem::take(&mut pending.rooms))
        };
        match overflow {
            Some(rooms) => self.publish(rooms),
            None => self.0.wake.notify_one(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        // The lock guards plain data and is never held across an await or a
        // panic-prone call, so a poisoned guard still holds consistent data.
        self.0
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn marks(&self) -> u64 {
        self.lock().marks
    }

    fn flush(&self) {
        let rooms = std::mem::take(&mut self.lock().rooms);
        self.publish(rooms);
    }

    /// Take the pending marks without publishing them, for tests that drive
    /// a producer without the flusher.
    #[cfg(test)]
    pub(crate) fn take_pending(&self) -> Rooms {
        std::mem::take(&mut self.lock().rooms)
    }

    fn publish(&self, rooms: Rooms) {
        let account_id = self.0.account_id;
        for (room_id, sources) in rooms {
            tracing::trace!(%account_id, %room_id, ?sources, "room details changed");
            // No subscriber is not an error; clients re-read on reconnect.
            let _ = self
                .0
                .live_tx
                .send(LiveFrame::RoomDetailsChanged(RoomDetailsFrame {
                    account_id,
                    room_id,
                    sources,
                }));
        }
    }
}

/// The account run's flusher, canceled and joined with the run. Pending
/// frames are published on cancellation rather than dropped.
pub(crate) async fn run(changes: RoomDetailsChanges, cancel: CancellationToken) {
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = changes.0.wake.notified() => {},
        }
        let first = Instant::now();
        let mut seen = changes.marks();
        loop {
            let wait = QUIET.min(MAX_DELAY.saturating_sub(first.elapsed()));
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    changes.flush();
                    return;
                }
                _ = tokio::time::sleep(wait) => {},
            }
            let marks = changes.marks();
            if marks == seen || first.elapsed() >= MAX_DELAY {
                break;
            }
            seen = marks;
        }
        changes.flush();
    }
    changes.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axon_core::RoomDetailSource::{MemberCounts, Metadata};

    fn received(rx: &mut broadcast::Receiver<LiveFrame>) -> Vec<(String, Vec<RoomDetailSource>)> {
        let mut frames = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            let LiveFrame::RoomDetailsChanged(frame) = frame else {
                panic!("unexpected frame {frame:?}");
            };
            frames.push((frame.room_id, frame.sources.into_iter().collect()));
        }
        frames.sort();
        frames
    }

    fn start() -> (
        RoomDetailsChanges,
        broadcast::Receiver<LiveFrame>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let (live_tx, rx) = broadcast::channel(1024);
        let changes = RoomDetailsChanges::new(Uuid::new_v4(), live_tx);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(changes.clone(), cancel.clone()));
        (changes, rx, cancel, task)
    }

    #[tokio::test(start_paused = true)]
    async fn one_room_batch_publishes_one_frame_with_the_union_of_sources() {
        let (changes, mut rx, cancel, task) = start();
        changes.mark("!a:x", Metadata);
        tokio::time::sleep(Duration::from_millis(20)).await;
        changes.mark("!a:x", Metadata);
        changes.mark("!a:x", MemberCounts);
        changes.mark("!b:x", Metadata);
        tokio::time::sleep(QUIET / 2).await;
        assert!(received(&mut rx).is_empty(), "published before quiet");
        tokio::time::sleep(QUIET * 2).await;
        assert_eq!(
            received(&mut rx),
            vec![
                ("!a:x".to_owned(), vec![Metadata, MemberCounts]),
                ("!b:x".to_owned(), vec![Metadata]),
            ]
        );
        tokio::time::sleep(MAX_DELAY * 2).await;
        assert!(received(&mut rx).is_empty(), "a room is published once");
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_marks_still_publish_within_the_maximum_delay() {
        let (changes, mut rx, cancel, task) = start();
        let started = Instant::now();
        let mut first_seen = None;
        while started.elapsed() < MAX_DELAY * 3 {
            changes.mark("!busy:x", Metadata);
            tokio::time::sleep(QUIET / 4).await;
            if first_seen.is_none() && !received(&mut rx).is_empty() {
                first_seen = Some(started.elapsed());
            }
        }
        let first_seen = first_seen.expect("continuous traffic starved the frame");
        assert!(first_seen <= MAX_DELAY + QUIET, "{first_seen:?}");
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_map_publishes_at_once() {
        let (changes, mut rx, cancel, task) = start();
        for room in 0..MAX_PENDING_ROOMS {
            changes.mark(&format!("!{room}:x"), Metadata);
        }
        assert_eq!(received(&mut rx).len(), MAX_PENDING_ROOMS);
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_publishes_pending_frames() {
        let (changes, mut rx, cancel, task) = start();
        changes.mark("!a:x", MemberCounts);
        tokio::task::yield_now().await;
        cancel.cancel();
        task.await.unwrap();
        assert_eq!(
            received(&mut rx),
            vec![("!a:x".to_owned(), vec![MemberCounts])]
        );
    }
}
