//! Bounded repair of singleton room state from the SDK's local, pruned cache.
//! Timeline log rows are hints, never proof that a redaction was applied.
use std::collections::HashSet;
use std::time::Duration;

use axon_store::{RoomStateRedaction, RoomStateUpsert};
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::{events::AnySyncStateEvent, OwnedRoomId, RoomId};
use matrix_sdk::{Client, RoomState};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::engine::{parse_raw_json, state_redaction_evidence, PersistContext};

/// Metadata plus the display state shared by /info, room lists, and spaces.
/// Never request a member list or enumerate state keys.
pub(crate) const STATE_TYPES: &[&str] = &[
    "m.room.canonical_alias",
    "m.room.create",
    "m.room.join_rules",
    "m.room.encryption",
    "m.room.power_levels",
    "m.room.server_acl",
    "m.room.history_visibility",
    "m.room.guest_access",
    "m.room.name",
    "m.room.topic",
    "m.room.avatar",
    "m.room.tombstone",
];

pub(crate) async fn watch(
    client: Client,
    ctx: PersistContext,
    mut hints: mpsc::Receiver<OwnedRoomId>,
    cancel: CancellationToken,
) {
    let mut tick = crate::room_sweep::interval();
    let mut sweep = crate::room_sweep::RoomSweep::new();
    // One paced worker per account: 32 queued hints and at most eight active
    // rooms (four sweep + four hint rooms), with 12 local reads per room.
    // Every tick advances the sweep, even under continuous hint traffic.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {},
        }
        let mut rooms = HashSet::new();
        rooms.extend(
            sweep
                .page(&ctx.store, ctx.account_id, &cancel, "state_redaction")
                .await,
        );
        rooms.extend(take_hints(&mut hints));
        for room_id in rooms {
            let Some(room) = client.get_room(&room_id) else {
                continue;
            };
            if room.state() != RoomState::Joined {
                continue;
            }
            let outcome = tokio::select! {
                _ = cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(2), reconcile_room(&ctx, &client, &room_id)) => result,
            };
            if outcome.is_err() {
                tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, "state redaction repair timed out; sweep will retry");
            }
        }
    }
}

/// Stop at the room budget, preserving the channel's tail for the next tick.
fn take_hints(hints: &mut mpsc::Receiver<OwnedRoomId>) -> HashSet<OwnedRoomId> {
    use crate::room_sweep::HintRead;
    crate::room_sweep::take_hints(|| match hints.try_recv() {
        Ok(room) => HintRead::Room(room),
        Err(mpsc::error::TryRecvError::Empty) => HintRead::Empty,
        Err(mpsc::error::TryRecvError::Disconnected) => HintRead::Closed,
    })
    .rooms
}

/// Ordinary message redactions need no SDK singleton reads. A failed filter
/// conservatively queues a hint; application of redaction is still SDK-only.
pub(crate) async fn queue_redaction_hint(ctx: &PersistContext, room_id: &RoomId, target: &str) {
    let matched = tokio::time::timeout(
        Duration::from_secs(2),
        ctx.store.is_state_reconciliation_target(
            ctx.account_id,
            room_id.as_str(),
            target,
            STATE_TYPES,
        ),
    )
    .await;
    match matched {
        Ok(Ok(false)) => return,
        Ok(Ok(true)) => {}
        _ => {
            tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, "state redaction hint filter failed; queuing conservative hint")
        }
    }
    let _ = ctx.state_redaction_tx.try_send(room_id.to_owned());
}

/// Normally hydrated state is already persisted. Read only that singleton to
/// detect the SDK pruning it without redispatch before the Axon write finished.
/// This also closes redaction-before-state ordering when the target filter saw
/// no current row yet, without filling the queue during ordinary initial sync.
pub(crate) async fn queue_state_hint(
    ctx: &PersistContext,
    client: &Client,
    room_id: &RoomId,
    event_type: &str,
    event_id: &str,
) {
    match tokio::time::timeout(
        Duration::from_secs(2),
        cached_redacted_state(ctx, client, room_id, event_type),
    )
    .await
    {
        Ok(Some((ev, _))) if ev.event_id().as_str() == event_id => {
            let _ = ctx.state_redaction_tx.try_send(room_id.to_owned());
        }
        Err(_) => {
            tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state hint check timed out; sweep will retry")
        }
        _ => {}
    }
}

async fn cached_redacted_state(
    ctx: &PersistContext,
    client: &Client,
    room_id: &RoomId,
    event_type: &str,
) -> Option<(AnySyncStateEvent, serde_json::Value)> {
    let raw = match client
        .state_store()
        .get_state_event(room_id, event_type.into(), "")
        .await
    {
        Ok(Some(RawAnySyncOrStrippedState::Sync(raw))) => raw,
        Ok(_) => return None,
        Err(_) => {
            tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state read failed");
            return None;
        }
    };
    // Bound our decode before allocating a second JSON representation.
    if raw.json().get().len() > 131072 {
        tracing::debug!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state exceeds reconciliation budget");
        return None;
    }
    let ev = raw.deserialize().ok()?;
    if !ev.is_redacted() {
        return None;
    }
    let value = parse_raw_json(raw.json().get(), ctx.account_id, "cached redacted state")?;
    Some((ev, value))
}

async fn reconcile_room(ctx: &PersistContext, client: &Client, room_id: &OwnedRoomId) {
    for event_type in STATE_TYPES {
        let Some((ev, value)) = cached_redacted_state(ctx, client, room_id, event_type).await
        else {
            continue;
        };
        let RoomStateRedaction::Redacted { event_id } = state_redaction_evidence(&ev, &value)
        else {
            continue;
        };
        let state = RoomStateUpsert {
            account_id: ctx.account_id,
            room_id: room_id.as_str(),
            event_type,
            state_key: "",
            event_id: ev.event_id().as_str(),
            sender: ev.sender().as_str(),
            origin_ts: i64::try_from(u64::from(ev.origin_server_ts().0)).unwrap_or(i64::MAX),
            content: value.get("content").cloned(),
        };
        match ctx
            .store
            .reconcile_redacted_room_state(&state, event_id)
            .await
        {
            Ok(true) => {
                tracing::debug!(account_id = %ctx.account_id, room_id = %room_id, event_type, event_id = %ev.event_id(), "reconciled redacted room state from SDK cache")
            }
            Ok(false) => {}
            Err(_) => {
                tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "redacted state persistence failed; sweep will retry")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{events::AnySyncStateEvent, serde::Raw};
    use matrix_sdk::{RoomInfo, StateChanges};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn hint_burst_preserves_tail_and_coalesces_duplicates() {
        let (tx, mut rx) = mpsc::channel(32);
        let rooms: Vec<OwnedRoomId> = (0..8)
            .map(|i| format!("!hint-{i}:localhost").parse().unwrap())
            .collect();
        for room in [
            &rooms[0], &rooms[0], &rooms[1], &rooms[2], &rooms[3], &rooms[4], &rooms[5], &rooms[6],
            &rooms[7],
        ] {
            tx.try_send(room.clone()).unwrap();
        }
        assert_eq!(take_hints(&mut rx), rooms[..4].iter().cloned().collect());
        assert_eq!(rx.len(), 4);
        assert_eq!(take_hints(&mut rx), rooms[4..].iter().cloned().collect());
        assert!(rx.is_empty());
    }

    #[test]
    fn missing_marker_never_asserts_original_content() {
        for content in [json!({}), json!({"ban": 50})] {
            let raw = json!({"type":"m.room.power_levels", "state_key":"",
                "event_id":"$state:localhost", "sender":"@alice:localhost",
                "origin_server_ts":10, "content":content});
            let ev: AnySyncStateEvent = serde_json::from_value(raw.clone()).unwrap();
            assert!(matches!(
                state_redaction_evidence(&ev, &raw),
                RoomStateRedaction::Unknown
            ));
        }
    }

    /// Exercise the consumer against an actual SDK state store and PostgreSQL,
    /// without redispatching the state event after the SDK applies a redaction.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn sdk_pruned_cache_repairs_shared_state_without_state_redispatch() {
        let store = axon_store::Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let account = store
            .upsert_account(
                &format!("@sdk-redaction-{}:localhost", Uuid::new_v4()),
                "https://hs.example.org",
            )
            .await
            .unwrap();
        let client = Client::builder()
            .homeserver_url("http://127.0.0.1:1")
            .server_versions([matrix_sdk::ruma::api::MatrixVersion::V1_11])
            .build()
            .await
            .unwrap();
        let room_id: OwnedRoomId = "!sdk-cache:localhost".parse().unwrap();
        let raw_value = json!({"type":"m.room.power_levels", "state_key":"",
            "event_id":"$state:localhost", "sender":"@alice:localhost",
            "origin_server_ts":10, "content":{"ban":50,"invite":99}});
        let raw: Raw<AnySyncStateEvent> = Raw::from_json_string(raw_value.to_string()).unwrap();
        let ev = raw.deserialize().unwrap();
        let mut changes = StateChanges::default();
        changes.add_room(RoomInfo::new(&room_id, RoomState::Joined));
        changes.add_state_event(&room_id, ev, raw);
        client.state_store().save_changes(&changes).await.unwrap();
        let state = RoomStateUpsert {
            account_id: account.account_id,
            room_id: room_id.as_str(),
            event_type: "m.room.power_levels",
            state_key: "",
            event_id: "$state:localhost",
            sender: "@alice:localhost",
            origin_ts: 10,
            content: Some(raw_value["content"].clone()),
        };
        store.upsert_room_state(&state).await.unwrap();
        let (state_redaction_tx, mut state_redaction_rx) = mpsc::channel(32);
        let (live_tx, _) = tokio::sync::broadcast::channel(8);
        let ctx = PersistContext {
            store: store.clone(),
            account_id: account.account_id,
            live_tx,
            index: None,
            local_user_id: std::sync::Arc::from(account.user_id.as_str()),
            purge_on_leave: false,
            state_redaction_tx,
        };
        // Ordinary message redactions and normal initial state do not enqueue
        // a full room scan. A current singleton target does enqueue one.
        queue_redaction_hint(&ctx, &room_id, "$message:localhost").await;
        queue_state_hint(
            &ctx,
            &client,
            &room_id,
            "m.room.power_levels",
            state.event_id,
        )
        .await;
        assert!(state_redaction_rx.is_empty());
        queue_redaction_hint(&ctx, &room_id, state.event_id).await;
        assert_eq!(state_redaction_rx.try_recv().unwrap(), room_id);
        // A marker-free form does not overwrite or certify existing content.
        reconcile_room(&ctx, &client, &room_id).await;
        assert_eq!(
            store
                .room_state(
                    account.account_id,
                    room_id.as_str(),
                    "m.room.power_levels",
                    ""
                )
                .await
                .unwrap()
                .unwrap()
                .redacted,
            None
        );
        let mut changes = StateChanges::default();
        changes.add_redaction(
            &room_id,
            "$state:localhost".try_into().unwrap(),
            Raw::from_json_string(
                json!({"type":"m.room.redaction", "event_id":"$redaction:localhost",
                "sender":"@alice:localhost", "origin_server_ts":11,
                "redacts":"$state:localhost", "content":{}})
                .to_string(),
            )
            .unwrap(),
        );
        client.state_store().save_changes(&changes).await.unwrap();
        // If filtering ran before the Axon tuple existed, the later original
        // state write detects the already-pruned SDK cache and queues recovery.
        queue_state_hint(
            &ctx,
            &client,
            &room_id,
            "m.room.power_levels",
            state.event_id,
        )
        .await;
        assert_eq!(state_redaction_rx.try_recv().unwrap(), room_id);
        queue_state_hint(
            &ctx,
            &client,
            &room_id,
            "m.room.power_levels",
            "$newer:localhost",
        )
        .await;
        assert!(state_redaction_rx.is_empty());
        // This is the worker's actual consumer; no persist_room_state_event call.
        reconcile_room(&ctx, &client, &room_id).await;
        let row = store
            .room_state(
                account.account_id,
                room_id.as_str(),
                "m.room.power_levels",
                "",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.redacted, Some(true));
        assert_eq!(
            row.redaction_event_id.as_deref(),
            Some("$redaction:localhost")
        );
        assert_eq!(row.content.as_ref().unwrap()["ban"], 50);
        let raw = client
            .state_store()
            .get_state_event(&room_id, "m.room.power_levels".into(), "")
            .await
            .unwrap()
            .unwrap();
        let RawAnySyncOrStrippedState::Sync(raw) = raw else {
            panic!("sync form")
        };
        let sdk_value: serde_json::Value = serde_json::from_str(raw.json().get()).unwrap();
        assert_eq!(row.content, Some(sdk_value["content"].clone()));
        // Repeated repair is idempotent; it cannot recreate purged state.
        reconcile_room(&ctx, &client, &room_id).await;
        // The SDK cache still holds the old redacted event. Even though its
        // content differs, it must not touch a newer Axon current-state event.
        let replacement = RoomStateUpsert {
            event_id: "$replacement:localhost",
            origin_ts: 20,
            content: Some(json!({"ban":80,"invite":40})),
            ..state
        };
        store.upsert_room_state(&replacement).await.unwrap();
        reconcile_room(&ctx, &client, &room_id).await;
        let current = store
            .room_state(
                account.account_id,
                room_id.as_str(),
                "m.room.power_levels",
                "",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.event_id, replacement.event_id);
        assert_eq!(current.content, replacement.content);
        assert_eq!(current.origin_ts, replacement.origin_ts);
        assert_eq!(current.redacted, None);
        assert_eq!(current.redaction_event_id, None);
        store
            .purge_room(account.account_id, room_id.as_str())
            .await
            .unwrap();
        reconcile_room(&ctx, &client, &room_id).await;
        assert!(store
            .room_state(
                account.account_id,
                room_id.as_str(),
                "m.room.power_levels",
                ""
            )
            .await
            .unwrap()
            .is_none());
        store.delete_account_row(account.account_id).await.unwrap();
    }
}
