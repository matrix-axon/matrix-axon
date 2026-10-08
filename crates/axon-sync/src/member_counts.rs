//! Paced reconciliation of SDK summary counts (ADR 0111, issue #620).
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use axon_store::{MemberCountWrite, RoomMemberCounts, Store};
use matrix_sdk::{
    ruma::{OwnedRoomId, RoomId},
    Client, RoomInfo, RoomState,
};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// Only explicitly supplied counts from the current membership epoch qualify.
// The lazy member list and the SDK's legacy zero defaults are never evidence.
fn snapshot(info: &RoomInfo) -> Option<RoomMemberCounts> {
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
        observed_at: u64::from(matrix_sdk::ruma::MilliSecondsSinceUnixEpoch::now().get())
            .try_into()
            .ok()?,
    })
}

/// Unknown/cold SDK state leaves the last timestamped observation intact.
async fn reconcile(client: &Client, store: &Store, account_id: Uuid, room_id: &RoomId) -> bool {
    let Some(room) = client.get_room(room_id) else {
        return false;
    };
    persist_snapshot(&room.clone_info(), store, account_id, room_id).await
}

async fn persist_snapshot(
    info: &RoomInfo,
    store: &Store,
    account_id: Uuid,
    room_id: &RoomId,
) -> bool {
    let result = if info.state() != RoomState::Joined {
        store
            .invalidate_room_member_counts(
                account_id,
                room_id.as_str(),
                u64::from(matrix_sdk::ruma::MilliSecondsSinceUnixEpoch::now().get()) as i64,
            )
            .await
    } else if let Some(counts) = snapshot(info) {
        store
            .set_room_member_counts(account_id, room_id.as_str(), counts)
            .await
    } else if let Some(invalidated_at) = info.summary_counts_invalidated_at() {
        store
            .invalidate_room_member_counts(
                account_id,
                room_id.as_str(),
                u64::from(invalidated_at.get()) as i64,
            )
            .await
    } else {
        return false;
    };
    match result {
        Ok(outcome) => {
            tracing::trace!(%account_id, %room_id, source = "sdk_summary", ?outcome, "reconciled member count observation");
            matches!(
                outcome,
                MemberCountWrite::Retry | MemberCountWrite::Superseded
            )
        }
        Err(error) => {
            tracing::warn!(%account_id, %room_id, source = "sdk_summary", reason = error.diagnostic_reason(), "member count persistence failed; will retry");
            true
        }
    }
}

/// A fixed queue closes SDK-before-projection races without retaining a map
/// proportional to account size. Overflow and exhausted retries fall back to
/// the progressive sweep; each retry recaptures current SDK state.
#[derive(Default)]
struct Pending(VecDeque<(OwnedRoomId, u8)>);

impl Pending {
    fn push(&mut self, room: OwnedRoomId, attempts: u8) {
        if attempts < 4 && self.0.len() < 32 && !self.0.iter().any(|(id, _)| id == &room) {
            self.0.push_back((room, attempts));
        }
    }
}

struct Worker {
    sweep: crate::room_sweep::RoomSweep,
    pending: Pending,
}

impl Worker {
    fn new() -> Self {
        Self {
            sweep: crate::room_sweep::RoomSweep::new(),
            pending: Pending::default(),
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
        let Some(hinted) = take_hints(receive, &mut self.sweep) else {
            return false;
        };
        for room in hinted {
            self.pending.push(room, 0);
        }
        let mut rooms = self
            .pending
            .0
            .drain(..self.pending.0.len().min(4))
            .collect::<Vec<_>>();
        for room in self
            .sweep
            .page(store, account_id, cancel, "member_counts")
            .await
        {
            if !rooms.iter().any(|(id, _)| id == &room) {
                rooms.push((room, 0));
            }
        }
        for (room_id, attempts) in rooms {
            let retry = tokio::select! {
                _ = cancel.cancelled() => return false,
                result = tokio::time::timeout(Duration::from_secs(2), reconcile(client, store, account_id, &room_id)) => {
                    match result {
                        Ok(retry) => retry,
                        Err(_) => {
                            tracing::warn!(%account_id, %room_id, source = "sdk_summary", reason = "deadline", "member count observation timed out; will retry");
                            true
                        }
                    }
                }
            };
            if retry {
                self.pending.push(room_id, attempts + 1);
            }
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
) {
    // Subscribe before the first page; receiver capacity is SDK-owned. Never
    // drain an unbounded update burst or retain an account-sized dedup map.
    let mut updates = client.room_info_notable_update_receiver();
    let mut tick = crate::room_sweep::interval();
    let mut worker = Worker::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = refresh.notified() => { worker.sweep.wake(); continue; },
            _ = tick.tick() => {},
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
            pending.push(format!("!retry-{i}:localhost").parse().unwrap(), 0);
        }
        assert_eq!(pending.0.len(), 32);
        let id = pending.0.front().unwrap().0.clone();
        pending.push(id, 0);
        assert_eq!(pending.0.len(), 32);
        pending.0.clear();
        pending.push(room_id!("!exhausted:localhost").to_owned(), 4);
        assert!(pending.0.is_empty());
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
    async fn joined_epoch_without_counts_invalidates_previous_projection() {
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
        let mut info = info(500, 3);
        assert!(
            !persist_snapshot(
                &info,
                &store,
                account.account_id,
                room_id!("!counts:localhost")
            )
            .await
        );
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_some());
        info.mark_as_left();
        info.mark_as_joined();
        assert!(
            !persist_snapshot(
                &info,
                &store,
                account.account_id,
                room_id!("!counts:localhost")
            )
            .await
        );
        assert!(store
            .room_member_counts(account.account_id, "!counts:localhost")
            .await
            .unwrap()
            .is_none());
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
        let mut worker = Worker::new();
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
        store.delete_account_row(account.account_id).await.unwrap();
        reconcile(
            &client,
            &store,
            account.account_id,
            room_id!("!counts:localhost"),
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
