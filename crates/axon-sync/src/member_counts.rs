//! Paced reconciliation of SDK summary counts (ADR 0111, issue #620).
use std::{collections::HashSet, sync::Arc, time::Duration};

use axon_store::{RoomMemberCounts, Store};
use matrix_sdk::{
    ruma::{OwnedRoomId, RoomId},
    Client, RoomInfo, RoomState,
};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// A joined room has at least ourselves. The SDK initializes absent summaries
// to zero: do not publish that sentinel as an authoritative empty room.
fn snapshot(info: &RoomInfo) -> Option<RoomMemberCounts> {
    if info.state() != RoomState::Joined || info.joined_members_count() == 0 {
        return None;
    }
    Some(RoomMemberCounts {
        joined: info.joined_members_count().try_into().ok()?,
        invited: info.invited_members_count().try_into().ok()?,
        observed_at: u64::from(matrix_sdk::ruma::MilliSecondsSinceUnixEpoch::now().get())
            .try_into()
            .ok()?,
    })
}

async fn reconcile(client: &Client, store: &Store, account_id: Uuid, room_id: &RoomId) {
    // One atomic RoomInfo snapshot; no remote I/O or membership enumeration.
    let counts = client
        .get_room(room_id)
        .and_then(|room| snapshot(&room.clone_info()));
    let outcome = if counts.is_some() {
        "observed"
    } else {
        "unknown"
    };
    if store
        .set_room_member_counts(account_id, room_id.as_str(), counts)
        .await
        .is_err()
    {
        tracing::warn!(%account_id, %room_id, source = "sdk_summary", "member count persistence failed; sweep will retry");
    } else {
        tracing::trace!(%account_id, %room_id, source = "sdk_summary", outcome, "reconciled member count observation");
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
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sweep = crate::room_sweep::RoomSweep::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = refresh.notified() => { sweep.wake(); continue; },
            _ = tick.tick() => {},
        }
        let mut rooms = HashSet::new();
        // Advance four keyset slots even under continuous live traffic.
        rooms.extend(sweep.page(&store, account_id, &cancel).await);
        let Some(hinted) = take_hints(
            || updates.try_recv().map(|update| update.room_id),
            &mut sweep,
        ) else {
            return;
        };
        rooms.extend(hinted);
        for room_id in rooms {
            tokio::select! {
                _ = cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(2), reconcile(&client, &store, account_id, &room_id)) => {
                    if result.is_err() {
                        tracing::warn!(%account_id, %room_id, source = "sdk_summary", "member count observation timed out; sweep will retry");
                    }
                }
            }
        }
    }
}

/// Share the exact receive-budget consumer with overflow/burst regressions.
fn take_hints(
    mut receive: impl FnMut() -> Result<OwnedRoomId, broadcast::error::TryRecvError>,
    sweep: &mut crate::room_sweep::RoomSweep,
) -> Option<HashSet<OwnedRoomId>> {
    let mut hinted = HashSet::new();
    for _ in 0..32 {
        if hinted.len() == 4 {
            break;
        }
        match receive() {
            Ok(room) => {
                hinted.insert(room);
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                sweep.wake();
                break;
            }
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Closed) => return None,
        }
    }
    Some(hinted)
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
        assert_eq!(snapshot(&info).unwrap().joined, 500);
        assert!(snapshot(&RoomInfo::new(
            room_id!("!empty:localhost"),
            RoomState::Joined
        ))
        .is_none());
        assert_eq!(snapshot(&self::info(499, 0)).unwrap().invited, 0);
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
        sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, '!counts:localhost', 1, '$fixture', 1, false)")
            .bind(account.account_id).execute(store.pool()).await.unwrap();
        let cancel = CancellationToken::new();
        let refresh = Arc::new(Notify::new());
        let task = tokio::spawn(watch(
            client.clone(),
            store.clone(),
            account.account_id,
            cancel.clone(),
            refresh.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(counts) = store
                    .room_member_counts(account.account_id, "!counts:localhost")
                    .await
                    .unwrap()
                {
                    assert_eq!((counts.joined, counts.invited), (500, 3));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        // Let the bounded scan become idle, then simulate the account run's
        // offline -> online signal with a lost projection write.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        store
            .set_room_member_counts(account.account_id, "!counts:localhost", None)
            .await
            .unwrap();
        refresh.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if store
                    .room_member_counts(account.account_id, "!counts:localhost")
                    .await
                    .unwrap()
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
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
