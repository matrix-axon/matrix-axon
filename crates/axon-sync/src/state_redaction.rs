//! Bounded repair of singleton room state from the SDK's local, pruned cache.
//! Timeline log rows are hints, never proof that a redaction was applied.
use std::collections::HashSet;
use std::time::Duration;

use axon_store::{RoomStateRedaction, RoomStateUpsert};
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::OwnedRoomId;
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
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cursor = String::new();
    let mut next_sweep = tokio::time::Instant::now();
    // One paced worker per account: 32 queued hints and at most eight active
    // rooms (four sweep + four hint rooms), with 12 local reads per room.
    // Every tick advances the sweep, even under continuous hint traffic.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {},
        }
        let mut rooms = HashSet::new();
        if tokio::time::Instant::now() >= next_sweep {
            let page = tokio::select! {
                _ = cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(2), ctx.store.state_reconciliation_rooms(ctx.account_id, &cursor)) => result,
            };
            match page {
                Ok(Ok(page)) => {
                    if let Some(last) = page.last() {
                        cursor.clone_from(last);
                    } else {
                        cursor.clear();
                        next_sweep = tokio::time::Instant::now() + Duration::from_secs(30);
                    }
                    rooms.extend(
                        page.into_iter()
                            .filter_map(|id| id.parse::<OwnedRoomId>().ok()),
                    );
                }
                _ => {
                    tracing::warn!(account_id = %ctx.account_id, "state redaction sweep page failed; will retry")
                }
            }
        }
        let mut hint_rooms = HashSet::new();
        for _ in 0..32 {
            let Ok(room) = hints.try_recv() else {
                break;
            };
            if hint_rooms.len() < 4 {
                hint_rooms.insert(room);
            }
            // Dropped overflow is healed by the paced sweep, including restart.
        }
        rooms.extend(hint_rooms);
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

async fn reconcile_room(ctx: &PersistContext, client: &Client, room_id: &OwnedRoomId) {
    for event_type in STATE_TYPES {
        let raw = match client
            .state_store()
            .get_state_event(room_id, (*event_type).into(), "")
            .await
        {
            Ok(Some(RawAnySyncOrStrippedState::Sync(raw))) => raw,
            Ok(_) => continue,
            Err(_) => {
                tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state read failed");
                continue;
            }
        };
        // Bound our decode before allocating a second JSON representation.
        if raw.json().get().len() > 131072 {
            tracing::debug!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state exceeds reconciliation budget");
            continue;
        }
        let Ok(ev) = raw.deserialize() else {
            continue;
        };
        let Some(value) = parse_raw_json(raw.json().get(), ctx.account_id, "cached redacted state")
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
        let (state_redaction_tx, _) = mpsc::channel(32);
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
