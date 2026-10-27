//! Paced reconciliation of SDK summary counts (ADR 0111, issue #620).
//!
//! Each per-room step also persists the SDK's calculated display name
//! (ADR 0118, [`crate::display_names`]), so one traversal covers both.
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use axon_core::{LiveFrame, RoomDetailSource};
use axon_store::{MemberCountWrite, RoomMemberCounts, Store};
use matrix_sdk::{
    ruma::{OwnedRoomId, RoomId},
    Client, Room, RoomInfo, RoomState,
};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::room_details::RoomDetailsChanges;

// Only explicitly supplied counts from the current membership epoch qualify.
// The lazy member list and the SDK's legacy zero defaults are never evidence.
fn snapshot(info: &RoomInfo, observed_at: i64) -> Option<RoomMemberCounts> {
    if info.state() != RoomState::Joined {
        return None;
    }
    let (joined, invited) = info.summary_member_counts();
    let joined = joined?;
    if joined == 0 {
        return None;
    }
    Some(RoomMemberCounts {
        joined: joined.try_into().ok()?,
        invited: invited?.try_into().ok()?,
        observed_at,
    })
}

/// The two halves of a room's reconcile step: the ADR 0111 counts and the
/// ADR 0118 display name. Each can fail on its own, so each retries on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Steps {
    counts: bool,
    display_name: bool,
}

impl Steps {
    const ALL: Self = Self {
        counts: true,
        display_name: true,
    };

    fn is_empty(self) -> bool {
        !self.counts && !self.display_name
    }

    fn union(self, other: Self) -> Self {
        Self {
            counts: self.counts || other.counts,
            display_name: self.display_name || other.display_name,
        }
    }
}

/// Run the requested halves, counts first, each under its own deadline so a
/// slow display-name read cannot drop the count write. Returns the halves to
/// retry.
async fn reconcile(
    client: &Client,
    store: &Store,
    account_id: Uuid,
    room_id: &RoomId,
    steps: Steps,
    live_tx: &broadcast::Sender<LiveFrame>,
    room_details: &RoomDetailsChanges,
) -> Steps {
    let Some(room) = client.get_room(room_id) else {
        return Steps::default();
    };
    let counts = steps.counts
        && deadline(
            account_id,
            room_id,
            "sdk_summary",
            reconcile_counts(&room, store, account_id, room_id, room_details),
        )
        .await;
    let display_name = steps.display_name
        && deadline(
            account_id,
            room_id,
            "sdk_display_name",
            crate::display_names::reconcile(&room, store, account_id, live_tx),
        )
        .await;
    Steps {
        counts,
        display_name,
    }
}

/// `work` reports whether to retry; running out of time is a retry too.
async fn deadline(
    account_id: Uuid,
    room_id: &RoomId,
    source: &'static str,
    work: impl std::future::Future<Output = bool>,
) -> bool {
    match tokio::time::timeout(Duration::from_secs(2), work).await {
        Ok(retry) => retry,
        Err(_) => {
            tracing::warn!(%account_id, %room_id, source, reason = "deadline", "room reconcile step timed out; will retry");
            true
        }
    }
}

/// Unknown/cold SDK state leaves the last timestamped observation intact.
async fn reconcile_counts(
    room: &Room,
    store: &Store,
    account_id: Uuid,
    room_id: &RoomId,
    room_details: &RoomDetailsChanges,
) -> bool {
    // Capture ordering before the SDK read, so a leave between this read and
    // persistence fences this snapshot using the same PostgreSQL clock.
    let observed_at = match store.member_count_observation_time().await {
        Ok(observed_at) => observed_at,
        Err(error) => {
            tracing::warn!(%account_id, %room_id, source = "sdk_summary", reason = error.diagnostic_reason(), "member count observation clock failed; will retry");
            return true;
        }
    };
    persist_snapshot(
        &room.clone_info(),
        store,
        account_id,
        room_id,
        observed_at,
        room_details,
    )
    .await
}

/// A stored or cleared pair announces a `member_counts` change (ADR 0126).
async fn persist_snapshot(
    info: &RoomInfo,
    store: &Store,
    account_id: Uuid,
    room_id: &RoomId,
    observed_at: i64,
    room_details: &RoomDetailsChanges,
) -> bool {
    let result = if let Some(counts) = snapshot(info, observed_at) {
        store
            .set_room_member_counts(account_id, room_id.as_str(), counts)
            .await
    } else if info.state() != RoomState::Joined
        || info.summary_member_counts().0 == Some(0)
        || info.summary_counts_invalidated_at().is_some()
    {
        store
            .invalidate_room_member_counts(account_id, room_id.as_str(), observed_at)
            .await
    } else {
        return false;
    };
    match result {
        Ok(outcome) => {
            tracing::trace!(%account_id, %room_id, source = "sdk_summary", ?outcome, "reconciled member count observation");
            if outcome == MemberCountWrite::Applied {
                room_details.mark(room_id.as_str(), RoomDetailSource::MemberCounts);
            }
            outcome == MemberCountWrite::Retry
        }
        Err(error) => {
            tracing::warn!(%account_id, %room_id, source = "sdk_summary", reason = error.diagnostic_reason(), "member count persistence failed; will retry");
            true
        }
    }
}

/// A fixed queue closes SDK-before-projection races without retaining a map
/// proportional to account size. Overflow and exhausted retries fall back to
/// the progressive sweep; each retry recaptures current SDK state and reruns
/// only the halves that failed.
#[derive(Default)]
struct Pending(VecDeque<(OwnedRoomId, Steps, u8)>);

impl Pending {
    fn push(&mut self, room: OwnedRoomId, steps: Steps, attempts: u8) {
        if steps.is_empty() {
            return;
        }
        if let Some((_, queued, _)) = self.0.iter_mut().find(|(id, _, _)| id == &room) {
            *queued = queued.union(steps);
        } else if attempts < 4 && self.0.len() < 32 {
            self.0.push_back((room, steps, attempts));
        }
    }
}

struct Worker {
    sweep: crate::room_sweep::RoomSweep,
    pending: Pending,
    live_tx: broadcast::Sender<LiveFrame>,
    room_details: RoomDetailsChanges,
}

impl Worker {
    fn new(live_tx: broadcast::Sender<LiveFrame>, room_details: RoomDetailsChanges) -> Self {
        Self {
            sweep: crate::room_sweep::RoomSweep::new(),
            pending: Pending::default(),
            live_tx,
            room_details,
        }
    }

    async fn step(
        &mut self,
        client: &Client,
        store: &Store,
        account_id: Uuid,
        cancel: &CancellationToken,
        receive: impl FnMut() -> Result<OwnedRoomId, broadcast::error::TryRecvError>,
    ) -> bool {
        if cancel.is_cancelled() {
            return false;
        }
        let Some(hinted) = take_hints(receive, &mut self.sweep) else {
            tracing::warn!(%account_id, source = "sdk_summary", reason = "update_channel_closed", "member count watcher stopped because the SDK update channel closed");
            return false;
        };
        for room in hinted {
            self.pending.push(room, Steps::ALL, 0);
        }
        let mut rooms = self
            .pending
            .0
            .drain(..self.pending.0.len().min(4))
            .collect::<Vec<_>>();
        let Some(page) = self
            .sweep
            .page(store, account_id, cancel, "member_counts")
            .await
        else {
            return false;
        };
        for room in page {
            match rooms.iter_mut().find(|(id, _, _)| id == &room) {
                Some((_, steps, _)) => *steps = Steps::ALL,
                None => rooms.push((room, Steps::ALL, 0)),
            }
        }
        for (room_id, steps, attempts) in rooms {
            let retry = tokio::select! {
                biased;
                _ = cancel.cancelled() => return false,
                retry = reconcile(client, store, account_id, &room_id, steps, &self.live_tx, &self.room_details) => retry,
            };
            self.pending.push(room_id, retry, attempts + 1);
        }
        true
    }
}

pub(crate) async fn watch(
    client: Client,
    store: Store,
    account_id: Uuid,
    cancel: CancellationToken,
    refresh: Arc<Notify>,
    live_tx: broadcast::Sender<LiveFrame>,
    room_details: RoomDetailsChanges,
) {
    // Subscribe before the first page; receiver capacity is SDK-owned. Never
    // drain an unbounded update burst or retain an account-sized dedup map.
    let mut updates = client.room_info_notable_update_receiver();
    let mut tick = crate::room_sweep::interval();
    let mut worker = Worker::new(live_tx, room_details);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {},
            _ = refresh.notified() => { worker.sweep.wake(); continue; },
        }
        if !worker
            .step(&client, &store, account_id, &cancel, || {
                updates.try_recv().map(|update| update.room_id)
            })
            .await
        {
            return;
        }
    }
}

/// Share the exact receive-budget consumer with overflow/burst regressions.
fn take_hints(
    mut receive: impl FnMut() -> Result<OwnedRoomId, broadcast::error::TryRecvError>,
    sweep: &mut crate::room_sweep::RoomSweep,
) -> Option<HashSet<OwnedRoomId>> {
    use crate::room_sweep::HintRead;
    let batch = crate::room_sweep::take_hints(|| match receive() {
        Ok(room) => HintRead::Room(room),
        Err(broadcast::error::TryRecvError::Lagged(_)) => HintRead::Lagged,
        Err(broadcast::error::TryRecvError::Empty) => HintRead::Empty,
        Err(broadcast::error::TryRecvError::Closed) => HintRead::Closed,
    });
    if batch.lagged {
        sweep.wake();
    }
    if batch.closed {
        None
    } else {
        Some(batch.rooms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::{
        authentication::matrix::MatrixSession,
        ruma::{api::client::sync::sync_events::v3::RoomSummary, room_id, user_id},
        store::RoomLoadSettings,
        SessionMeta, SessionTokens, StateChanges,
    };

    fn snapshot(info: &RoomInfo) -> Option<RoomMemberCounts> {
        super::snapshot(info, 1)
    }

    #[test]
    fn closed_update_channel_ends_hint_consumption() {
        let mut sweep = crate::room_sweep::RoomSweep::new();
        assert!(take_hints(|| Err(broadcast::error::TryRecvError::Closed), &mut sweep).is_none());
    }

    #[tokio::test]
    async fn hint_bursts_and_lag_are_bounded_and_preserve_tail() {
        let mut sweep = crate::room_sweep::RoomSweep::new();
        let (tx, mut rx) = broadcast::channel(64);
        for i in 0..8 {
            tx.send(
                format!("!hint-{i}:localhost")
                    .parse::<OwnedRoomId>()
                    .unwrap(),
            )
            .unwrap();
        }
        assert_eq!(take_hints(|| rx.try_recv(), &mut sweep).unwrap().len(), 4);
        assert_eq!(rx.len(), 4);
        assert_eq!(take_hints(|| rx.try_recv(), &mut sweep).unwrap().len(), 4);
        // Duplicate traffic cannot monopolize a tick either.
        for _ in 0..40 {
            tx.send(room_id!("!duplicate:localhost").to_owned())
                .unwrap();
        }
        assert_eq!(take_hints(|| rx.try_recv(), &mut sweep).unwrap().len(), 1);
        assert_eq!(rx.len(), 8);
        for _ in 0..100 {
            tx.send(room_id!("!overflow:localhost").to_owned()).unwrap();
        }
        assert!(take_hints(|| rx.try_recv(), &mut sweep).unwrap().is_empty());
        assert_eq!(take_hints(|| rx.try_recv(), &mut sweep).unwrap().len(), 1);
    }

    #[test]
    fn retries_are_coalesced_and_bounded() {
        let mut pending = Pending::default();
        for i in 0..100 {
            pending.push(
                format!("!retry-{i}:localhost").parse().unwrap(),
                Steps::ALL,
                0,
            );
        }
        assert_eq!(pending.0.len(), 32);
        let id = pending.0.front().unwrap().0.clone();
        pending.push(id, Steps::ALL, 0);
        assert_eq!(pending.0.len(), 32);
        pending.0.clear();
        pending.push(room_id!("!exhausted:localhost").to_owned(), Steps::ALL, 4);
        assert!(pending.0.is_empty());
        pending.push(room_id!("!done:localhost").to_owned(), Steps::default(), 0);
        assert!(pending.0.is_empty(), "nothing failed, nothing to retry");
    }

    #[test]
    fn retries_keep_each_half_separate_and_merge_duplicates() {
        let counts = Steps {
            counts: true,
            display_name: false,
        };
        let display_name = Steps {
            counts: false,
            display_name: true,
        };
        let mut pending = Pending::default();
        pending.push(room_id!("!a:localhost").to_owned(), display_name, 1);
        assert_eq!(pending.0[0].1, display_name);
        pending.push(room_id!("!a:localhost").to_owned(), counts, 1);
        assert_eq!(pending.0.len(), 1);
        assert_eq!(pending.0[0].1, Steps::ALL);
    }

    fn info(joined: u32, invited: u32) -> RoomInfo {
        let mut info = RoomInfo::new(room_id!("!counts:localhost"), RoomState::Joined);
        let mut summary = RoomSummary::new();
        summary.joined_member_count = Some(joined.into());
        summary.invited_member_count = Some(invited.into());
        info.update_from_ruma_summary(&summary);
        info.mark_state_partially_synced();
        info
    }

    #[test]
    fn partial_membership_uses_summary_and_non_joined_is_unknown() {
        let mut info = info(500, 3);
        assert!(!info.are_members_synced());
        let counts = snapshot(&info).unwrap();
        assert_eq!((counts.joined, counts.invited), (500, 3));
        assert_eq!(counts.observed_at, 1, "use the supplied database clock");
        info.mark_as_left();
        assert!(snapshot(&info).is_none());
        info.mark_as_invited();
        assert!(snapshot(&info).is_none());
        info.mark_as_banned();
        assert!(snapshot(&info).is_none());
        info.mark_as_joined();
        assert!(
            snapshot(&info).is_none(),
            "rejoin must not reuse pre-leave counts"
        );
        let mut summary = RoomSummary::new();
        summary.joined_member_count = Some(499u32.into());
        info.update_from_ruma_summary(&summary);
        assert!(
            snapshot(&info).is_none(),
            "missing invited count is unknown"
        );
        summary.joined_member_count = None;
        summary.invited_member_count = Some(0u32.into());
        info.update_from_ruma_summary(&summary);
        assert_eq!(snapshot(&info).unwrap().invited, 0);
        assert!(snapshot(&RoomInfo::new(
            room_id!("!empty:localhost"),
            RoomState::Joined
        ))
        .is_none());
        assert_eq!(snapshot(&self::info(499, 0)).unwrap().invited, 0);
    }

    #[test]
    fn summary_availability_survives_serialization_and_legacy_is_unknown() {
        let current = info(500, 0);
        let encoded = serde_json::to_value(&current).unwrap();
        let restored: RoomInfo = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(restored.summary_member_counts(), (Some(500), Some(0)));
        let mut legacy = encoded;
        for field in [
            "summary_joined_count_known",
            "summary_invited_count_known",
            "summary_counts_invalidated_at",
        ] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        let restored: RoomInfo = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.joined_members_count(), 500);
        assert_eq!(restored.summary_member_counts(), (None, None));
        assert!(snapshot(&restored).is_none());
        let mut transitioned = current;
        transitioned.mark_as_left();
        transitioned.mark_as_joined();
        let restored: RoomInfo =
            serde_json::from_value(serde_json::to_value(&transitioned).unwrap()).unwrap();
        assert!(restored.summary_counts_invalidated_at().is_some());
        assert!(snapshot(&restored).is_none());
    }

    #[tokio::test]
    async fn sliding_sync_counts_preserve_presence_and_invalidate_on_rejoin() {
        use matrix_sdk::ruma::api::client::sync::sync_events::v5;
        use matrix_sdk_base::{
            cross_process_lock::CrossProcessLockConfig, store::StoreConfig, BaseClient,
            DmRoomDefinition, RequestedRequiredStates, ThreadingSupport,
        };
        let client = BaseClient::new(
            StoreConfig::new(CrossProcessLockConfig::SingleProcess),
            ThreadingSupport::Disabled,
            DmRoomDefinition::default(),
        );
        client
            .activate(
                matrix_sdk_base::SessionMeta {
                    user_id: user_id!("@counts:localhost").to_owned(),
                    device_id: "COUNTS".into(),
                },
                RoomLoadSettings::default(),
                None,
            )
            .await
            .unwrap();

        async fn apply(
            client: &BaseClient,
            joined: Option<u32>,
            invited: Option<u32>,
            membership: Option<&str>,
            initial: Option<bool>,
        ) -> RoomInfo {
            let mut room = v5::response::Room::new();
            room.joined_count = joined.map(Into::into);
            room.invited_count = invited.map(Into::into);
            room.initial = initial;
            if let Some(membership) = membership {
                room.required_state = serde_json::from_value(serde_json::json!([{
                    "type": "m.room.member", "state_key": "@counts:localhost",
                    "sender": "@counts:localhost", "event_id": format!("${membership}"),
                    "origin_server_ts": 1, "content": {"membership": membership}
                }]))
                .unwrap();
            }
            let mut response = v5::Response::new("fixture".to_owned());
            response
                .rooms
                .insert(room_id!("!counts:localhost").to_owned(), room);
            client
                .process_sliding_sync(
                    &response,
                    &RequestedRequiredStates::default(),
                    &client.state_store_lock().lock().await,
                )
                .await
                .unwrap();
            client
                .get_room(room_id!("!counts:localhost"))
                .unwrap()
                .clone_info()
        }

        let info = apply(&client, Some(500), None, Some("join"), None).await;
        assert_eq!(info.summary_member_counts(), (Some(500), None));
        assert!(
            snapshot(&info).is_none(),
            "omitted invites are not authoritative zero"
        );
        let info = apply(&client, None, Some(0), None, None).await;
        assert_eq!(snapshot(&info).unwrap().invited, 0);
        let info = apply(&client, None, None, None, None).await;
        assert_eq!(
            snapshot(&info).unwrap().joined,
            500,
            "same-epoch deltas retain known fields"
        );
        let info = apply(&client, None, None, Some("leave"), None).await;
        assert_eq!(info.state(), RoomState::Left);
        assert!(snapshot(&info).is_none());
        let info = apply(&client, None, None, Some("join"), None).await;
        assert_eq!(info.summary_member_counts(), (None, None));
        assert!(info.summary_counts_invalidated_at().is_some());
        assert!(snapshot(&info).is_none());
        assert!(snapshot(&apply(&client, Some(499), None, None, None).await).is_none());
        let info = apply(&client, None, Some(2), None, None).await;
        let counts = snapshot(&info).unwrap();
        assert_eq!((counts.joined, counts.invited), (499, 2));
        let info = apply(&client, None, None, None, Some(false)).await;
        assert_eq!(info.summary_member_counts(), (Some(499), Some(2)));
        // An initial replacement starts a new epoch even without a membership
        // change. Only counts actually present in that response remain known.
        let info = apply(&client, Some(498), None, None, Some(true)).await;
        assert_eq!(info.summary_member_counts(), (Some(498), None));
        assert!(info.summary_counts_invalidated_at().is_some());
        assert!(snapshot(&info).is_none());
        assert_eq!(
            snapshot(&apply(&client, None, Some(0), None, None).await)
                .unwrap()
                .invited,
            0
        );
        let info = apply(&client, None, None, None, Some(true)).await;
        assert_eq!(info.summary_member_counts(), (None, None));
        assert!(snapshot(&info).is_none());
        let info = apply(&client, Some(497), Some(1), None, Some(true)).await;
        assert_eq!(info.summary_member_counts(), (Some(497), Some(1)));
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn database_clock_fences_epochs_and_known_zero_clears_counts() {
        let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let account = store
            .upsert_account(
                &format!("@epoch-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .unwrap();
        sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, '!counts:localhost', 1, '$fixture', 1, false)")
            .bind(account.account_id).execute(store.pool()).await.unwrap();
        let (live_tx, _) = broadcast::channel(8);
        let changes = RoomDetailsChanges::new(account.account_id, live_tx);
        // Every applied write announces `member_counts`; nothing else does.
        let announced = |changes: &RoomDetailsChanges| {
            let pending = changes.take_pending();
            assert!(pending.len() <= 1, "{pending:?}");
            pending
                .get("!counts:localhost")
                .map(|sources| {
                    assert_eq!(
                        sources.iter().copied().collect::<Vec<_>>(),
                        [RoomDetailSource::MemberCounts]
                    );
                })
                .is_some()
        };
        let mut info = info(500, 3);
        assert!(
            !persist_snapshot(
                &info,
                &store,
                account.account_id,
                room_id!("!counts:localhost"),
                store.member_count_observation_time().await.unwrap(),
                &changes,
            )
            .await
        );
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_some());
        assert!(announced(&changes), "a first pair is a change");
        assert!(
            !persist_snapshot(
                &info,
                &store,
                account.account_id,
                room_id!("!counts:localhost"),
                store.member_count_observation_time().await.unwrap(),
                &changes,
            )
            .await
        );
        assert!(!announced(&changes), "an identical pair is not a change");
        info.mark_as_left();
        info.mark_as_joined();
        // An SDK invalidation time from a host clock far ahead must not leak
        // into the database ordering clock and block fresh rejoin counts.
        let mut encoded = serde_json::to_value(&info).unwrap();
        encoded["summary_counts_invalidated_at"] = serde_json::json!(4000000000000u64);
        let future_clock: RoomInfo = serde_json::from_value(encoded).unwrap();
        assert!(
            !persist_snapshot(
                &future_clock,
                &store,
                account.account_id,
                room_id!("!counts:localhost"),
                store.member_count_observation_time().await.unwrap(),
                &changes,
            )
            .await
        );
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_none());
        assert!(announced(&changes), "clearing a pair is a change");
        // Advance the database-clock observation by one millisecond rather
        // than relying on the application host's time or task scheduling.
        let now = store.member_count_observation_time().await.unwrap() + 1;
        assert!(
            !persist_snapshot(
                &self::info(3, 0),
                &store,
                account.account_id,
                room_id!("!counts:localhost"),
                now,
                &changes,
            )
            .await
        );
        assert_eq!(
            store
                .room_member_counts(account.account_id, "!counts:localhost")
                .await
                .unwrap()
                .unwrap()
                .joined,
            3
        );
        assert!(announced(&changes));
        // A server-reported zero is positive evidence even with no invited
        // count and no SDK transition marker. It must erase the old 3.
        let mut zero = RoomInfo::new(room_id!("!counts:localhost"), RoomState::Joined);
        let mut summary = RoomSummary::new();
        summary.joined_member_count = Some(0u32.into());
        zero.update_from_ruma_summary(&summary);
        assert!(zero.summary_counts_invalidated_at().is_none());
        assert!(
            !persist_snapshot(
                &zero,
                &store,
                account.account_id,
                room_id!("!counts:localhost"),
                now + 1,
                &changes,
            )
            .await
        );
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_none());
        assert!(announced(&changes));
        // Superseded observations are terminal, never queued as retries.
        assert!(
            !persist_snapshot(
                &self::info(4, 0),
                &store,
                account.account_id,
                room_id!("!counts:localhost"),
                now,
                &changes,
            )
            .await
        );
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_none());
        assert!(
            !announced(&changes),
            "a superseded observation is not a change"
        );
        store.delete_account_row(account.account_id).await.unwrap();
    }

    // Real SDK cache -> actual watcher -> real PostgreSQL, including startup
    // without any fresh sync event and periodic recovery after dropped hints.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn startup_and_sweep_repair_sdk_counts_without_member_hydration() {
        let store = Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let account = store
            .upsert_account(
                &format!("@counts-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .unwrap();
        let sdk_dir = std::env::temp_dir().join(format!("axon-counts-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let build = || {
            Client::builder()
                .homeserver_url("http://127.0.0.1:1")
                .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
                .sqlite_store_with_config_and_cache_path(
                    matrix_sdk::SqliteStoreConfig::new(&sdk_dir),
                    None::<&std::path::Path>,
                )
                .build()
        };
        let client = build().await.unwrap();
        let mut changes = StateChanges::default();
        changes.add_room(info(500, 3));
        changes.add_room(RoomInfo::new(
            room_id!("!counts-0:localhost"),
            RoomState::Joined,
        ));
        client.state_store().save_changes(&changes).await.unwrap();
        drop(client);
        // Reopen the persistent SDK cache before restoring the session.
        let client = build().await.unwrap();
        client
            .matrix_auth()
            .restore_session(
                MatrixSession {
                    meta: SessionMeta {
                        user_id: user_id!("@counts:localhost").to_owned(),
                        device_id: "COUNTS".into(),
                    },
                    tokens: SessionTokens {
                        access_token: "fixture".into(),
                        refresh_token: None,
                    },
                },
                RoomLoadSettings::default(),
            )
            .await
            .unwrap();
        // Seed summaries without a single member event or upstream request.
        for i in 0..9 {
            sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, $2, 1, '$fixture', 1, false)")
                .bind(account.account_id).bind(format!("!counts-{i}:localhost")).execute(store.pool()).await.unwrap();
        }
        let cancel = CancellationToken::new();
        let (live_tx, mut live_rx) = broadcast::channel(64);
        let room_details = RoomDetailsChanges::new(account.account_id, live_tx.clone());
        let mut worker = Worker::new(live_tx.clone(), room_details.clone());
        let empty = || Err(broadcast::error::TryRecvError::Empty);
        // SDK update before Axon's projection: retain a bounded retry.
        let mut hint = Some(room_id!("!counts:localhost").to_owned());
        assert!(
            worker
                .step(&client, &store, account.account_id, &cancel, || hint
                    .take()
                    .ok_or(broadcast::error::TryRecvError::Empty))
                .await
        );
        assert_eq!(worker.pending.0.len(), 1);
        assert_eq!(
            worker.pending.0[0].1,
            Steps::ALL,
            "no summary row fails both"
        );
        sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, '!counts:localhost', 1, '$fixture', 1, false)")
            .bind(account.account_id).execute(store.pool()).await.unwrap();
        // Both a cold SDK summary and a missing SDK room preserve cached counts.
        for room in ["!counts-0:localhost", "!counts-1:localhost"] {
            store
                .set_room_member_counts(
                    account.account_id,
                    room,
                    RoomMemberCounts {
                        joined: 23,
                        invited: 1,
                        observed_at: 1,
                    },
                )
                .await
                .unwrap();
        }
        for _ in 0..4 {
            assert!(
                worker
                    .step(&client, &store, account.account_id, &cancel, empty)
                    .await
            );
        }
        assert!(worker.pending.0.is_empty());
        for room in ["!counts-0:localhost", "!counts-1:localhost"] {
            assert_eq!(
                store
                    .room_member_counts(account.account_id, room)
                    .await
                    .unwrap()
                    .unwrap()
                    .observed_at,
                1
            );
        }
        assert!(worker.sweep.is_idle());
        let counts = store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .unwrap();
        assert_eq!((counts.joined, counts.invited), (500, 3));
        let announced = room_details.take_pending();
        assert!(announced.contains_key("!counts:localhost"));
        assert!(
            !announced.contains_key("!counts-0:localhost")
                && !announced.contains_key("!counts-1:localhost"),
            "preserved counts are not a change: {announced:?}"
        );
        // Rejoin SDK update before the local membership handler: a hidden
        // summary must retain its hint until the projection becomes joined.
        sqlx_core::query::query("UPDATE room_summaries SET hidden_left = true WHERE account_id = $1 AND room_id = '!counts:localhost'")
            .bind(account.account_id).execute(store.pool()).await.unwrap();
        let mut hint = Some(room_id!("!counts:localhost").to_owned());
        assert!(
            worker
                .step(&client, &store, account.account_id, &cancel, || hint
                    .take()
                    .ok_or(broadcast::error::TryRecvError::Empty))
                .await
        );
        assert_eq!(worker.pending.0.len(), 1);
        assert_eq!(
            worker.pending.0[0].1,
            Steps {
                counts: true,
                display_name: false
            },
            "the stored display name is not retried with the counts"
        );
        let (watermark,): (i64,) = sqlx_core::query_as::query_as("SELECT member_counts_observed_at FROM room_summaries WHERE account_id = $1 AND room_id = '!counts:localhost'")
            .bind(account.account_id).fetch_one(store.pool()).await.unwrap();
        sqlx_core::query::query("UPDATE room_summaries SET hidden_left = false WHERE account_id = $1 AND room_id = '!counts:localhost'")
            .bind(account.account_id).execute(store.pool()).await.unwrap();
        // Wait for the timestamp precondition, not an assumed worker schedule.
        tokio::time::timeout(Duration::from_secs(1), async {
            while u64::from(matrix_sdk::ruma::MilliSecondsSinceUnixEpoch::now().get())
                <= watermark as u64
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            worker
                .step(&client, &store, account.account_id, &cancel, empty)
                .await
        );
        assert!(worker.pending.0.is_empty());
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_some());
        // Simulate a lost projection while the actual consumer is proven idle.
        sqlx_core::query::query("UPDATE room_summaries SET joined_member_count = NULL, invited_member_count = NULL, member_counts_observed_at = NULL WHERE account_id = $1 AND room_id = '!counts:localhost'")
            .bind(account.account_id).execute(store.pool()).await.unwrap();
        worker.sweep.wake();
        assert!(!worker.sweep.is_idle());
        for _ in 0..4 {
            assert!(
                worker
                    .step(&client, &store, account.account_id, &cancel, empty)
                    .await
            );
        }
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_some());
        // The same traversal stored the SDK's calculated display name
        // (ADR 0118): no heroes, 503 joined + invited.
        let (display_name,): (Option<String>,) = sqlx_core::query_as::query_as("SELECT display_name FROM room_summaries WHERE account_id = $1 AND room_id = '!counts:localhost'")
            .bind(account.account_id).fetch_one(store.pool()).await.unwrap();
        assert_eq!(display_name.as_deref(), Some("503 people"));
        let mut announced = Vec::new();
        while let Ok(frame) = live_rx.try_recv() {
            if let LiveFrame::RoomDisplayNameChanged(frame) = frame {
                announced.push((frame.room_id, frame.display_name));
            }
        }
        assert!(announced.contains(&("!counts:localhost".to_owned(), "503 people".to_owned())));
        store.delete_account_row(account.account_id).await.unwrap();
        reconcile(
            &client,
            &store,
            account.account_id,
            room_id!("!counts:localhost"),
            Steps::ALL,
            &live_tx,
            &room_details,
        )
        .await;
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_none());
        drop(client);
        std::fs::remove_dir_all(sdk_dir).unwrap();
    }
}
