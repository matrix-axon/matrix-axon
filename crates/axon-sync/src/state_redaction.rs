//! Bounded repair of room state from the SDK cache and resolved redaction
//! events. A stored redaction alone is not proof it may be applied.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::time::Duration;

use axon_store::{
    RoomStateRedaction, RoomStateUpsert, UnappliedStateRedaction, STATE_REDACTION_PAGE_SIZE,
};
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::canonical_json::{redact, CanonicalJsonObject, RedactedBecause};
use matrix_sdk::ruma::events::room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent};
use matrix_sdk::ruma::room_version_rules::{RedactionRules, RoomVersionRules};
use matrix_sdk::ruma::{events::AnySyncStateEvent, OwnedRoomId, RoomId, UserId};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::store::StateStoreExt;
use matrix_sdk::{Client, RoomInfo, RoomState};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::engine::{
    mark_room_state_changes, parse_raw_json, state_redaction_evidence, PersistContext,
};

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

/// The [`STATE_TYPES`] only the supplementary connection requests (ADR 0126),
/// so the SDK cache may never hold them and repair falls back to the stored
/// tuple.
const SUPPLEMENTARY_STATE_TYPES: &[&str] = &["m.room.guest_access", "m.room.server_acl"];
// Keyed space links can also arrive only through supplementary sync. Repair
// their stored forms without enumerating the SDK's state keys.
const SPACE_LINK_TYPES: &[&str] = &["m.space.child", "m.space.parent"];

type RepairCursor = (String, String);

pub(crate) async fn watch(
    client: Client,
    ctx: PersistContext,
    mut hints: mpsc::Receiver<OwnedRoomId>,
    cancel: CancellationToken,
) {
    let mut tick = crate::room_sweep::interval();
    let mut sweep = crate::room_sweep::RoomSweep::new();
    let mut pending = BTreeMap::<OwnedRoomId, RepairCursor>::new();
    let mut last_resumed = None;
    // Each tick advances the room sweep and gives one pending room another
    // page in round-robin order, even when hints keep arriving.
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            _ = tick.tick() => {},
        }
        let Some(page) = sweep
            .page(&ctx.store, ctx.account_id, &cancel, "state_redaction", true)
            .await
        else {
            return;
        };
        let mut rooms: HashMap<_, _> = page.into_iter().collect();
        // Live hints can precede the next room-page probe.
        rooms.extend(take_hints(&mut hints).into_iter().map(|room| (room, true)));
        let next = last_resumed
            .as_ref()
            .and_then(|last| {
                pending
                    .range::<OwnedRoomId, _>((Bound::Excluded(last), Bound::Unbounded))
                    .next()
            })
            .or_else(|| pending.first_key_value());
        if let Some((room, _)) = next {
            rooms.insert(room.clone(), true);
            last_resumed = Some(room.clone());
        }
        for (room_id, check_space_links) in rooms {
            if cancel.is_cancelled() {
                return;
            }
            let mut after = pending.remove(&room_id);
            let Some(room) = client.get_room(&room_id) else {
                continue;
            };
            if room.state() != RoomState::Joined {
                continue;
            }
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                result = tokio::time::timeout(Duration::from_secs(2), reconcile_room(&ctx, &client, &room_id, check_space_links, &mut after)) => result,
            };
            if let Some(after) = after {
                pending.insert(room_id.clone(), after);
            }
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
        cached_state(ctx, client, room_id, event_type),
    )
    .await
    {
        Ok(Cached::Redacted(ev, _)) if ev.event_id().as_str() == event_id => {
            let _ = ctx.state_redaction_tx.try_send(room_id.to_owned());
        }
        Err(_) => {
            tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state hint check timed out; sweep will retry")
        }
        _ => {}
    }
}

/// What the SDK cache holds for one singleton.
enum Cached {
    /// The SDK never stored this state.
    Missing,
    /// A form repair cannot use: original, oversized, undecodable, or a
    /// failed read.
    Unusable,
    /// The SDK's redacted form.
    Redacted(Box<AnySyncStateEvent>, serde_json::Value),
}

async fn cached_state(
    ctx: &PersistContext,
    client: &Client,
    room_id: &RoomId,
    event_type: &str,
) -> Cached {
    let raw = match client
        .state_store()
        .get_state_event(room_id, event_type.into(), "")
        .await
    {
        Ok(Some(RawAnySyncOrStrippedState::Sync(raw))) => raw,
        Ok(None) => return Cached::Missing,
        Ok(Some(RawAnySyncOrStrippedState::Stripped(_))) => return Cached::Unusable,
        Err(_) => {
            tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state read failed");
            return Cached::Unusable;
        }
    };
    // Bound our decode before allocating a second JSON representation.
    if raw.json().get().len() > 131072 {
        tracing::debug!(account_id = %ctx.account_id, room_id = %room_id, event_type, "SDK cached state exceeds reconciliation budget");
        return Cached::Unusable;
    }
    let Ok(ev) = raw.deserialize() else {
        return Cached::Unusable;
    };
    if !ev.is_redacted() {
        return Cached::Unusable;
    }
    match parse_raw_json(raw.json().get(), ctx.account_id, "cached redacted state") {
        Some(value) => Cached::Redacted(Box::new(ev), value),
        None => Cached::Unusable,
    }
}

/// Redact Axon's stored tuples of `event_types`, which the SDK cache never
/// held, for each stored redaction event that targets one (ADR 0126). The
/// supplementary connection is the only source of these tuples and never
/// redelivers a redacted target, so nothing else would apply the redaction.
/// This is what the SDK store does to the state it holds: ruma's `redact`
/// under the room's version rules, with the same trust in a synced redaction.
async fn reconcile_stored_state(
    ctx: &PersistContext,
    client: &Client,
    room_id: &OwnedRoomId,
    event_types: &[&str],
    after: &mut Option<RepairCursor>,
) {
    let candidates = match ctx
        .store
        .unapplied_room_state_redactions(
            ctx.account_id,
            room_id.as_str(),
            event_types,
            after
                .as_ref()
                .map(|(kind, key)| (kind.as_str(), key.as_str())),
        )
        .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(account_id = %ctx.account_id, %room_id, reason = error.diagnostic_reason(), "stored state redaction read failed; sweep will retry");
            return;
        }
    };
    if candidates.is_empty() {
        *after = None;
        return;
    }
    let more = candidates.len() == STATE_REDACTION_PAGE_SIZE;
    let info = client
        .state_store()
        .get_room_infos(&RoomLoadSettings::One(room_id.clone()))
        .await;
    let Some(info) = info.ok().and_then(|infos| infos.into_iter().next()) else {
        tracing::debug!(account_id = %ctx.account_id, %room_id, "no SDK room info for stored state redaction; sweep will retry");
        return;
    };
    let rules = info.room_version_rules_or_default();
    let levels = power_levels(client, room_id, &info, &rules).await;
    for candidate in candidates {
        let event_type = candidate.event_type.clone();
        // Advance before the write so a locked tuple cannot starve the tail.
        // Unapplied tuples are eligible again when the next sweep wraps.
        *after = Some((event_type.clone(), candidate.state_key.clone()));
        if !redaction_authorized(&candidate.redaction, &candidate.sender, levels.as_ref()) {
            tracing::debug!(account_id = %ctx.account_id, %room_id, event_type, "stored redaction is not authorized; left unapplied");
            continue;
        }
        if let Some((ev, value)) = redact_stored(candidate, &rules.redaction) {
            apply(ctx, room_id, &event_type, &ev, &value).await;
        } else {
            tracing::debug!(account_id = %ctx.account_id, %room_id, event_type, "stored state or its redaction is not redactable JSON");
        }
    }
    if !more {
        *after = None;
    }
}

/// The room's current power levels from the SDK cache, which the primary
/// connection keeps, as the SDK's own `Room::power_levels` computes them.
async fn power_levels(
    client: &Client,
    room_id: &RoomId,
    info: &RoomInfo,
    rules: &RoomVersionRules,
) -> Option<RoomPowerLevels> {
    let raw = client
        .state_store()
        .get_state_event_static::<RoomPowerLevelsEventContent>(room_id)
        .await
        .ok()??;
    let content = raw.deserialize().ok()?;
    Some(content.power_levels(&rules.authorization, info.creators()?))
}

/// Whether a client may apply `redaction` to state that `target_sender` sent.
/// Delivery is not authorization: from room version 3, a homeserver accepts
/// any redaction and applies it only once it confirms that the redaction's
/// sender may redact others' events or is on the target sender's server. This
/// is that check, against the room's current power levels; without them only
/// a same-server redaction applies.
fn redaction_authorized(
    redaction: &serde_json::Value,
    target_sender: &str,
    power_levels: Option<&RoomPowerLevels>,
) -> bool {
    let Some(sender) = redaction
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .and_then(|sender| <&UserId>::try_from(sender).ok())
    else {
        return false;
    };
    <&UserId>::try_from(target_sender)
        .is_ok_and(|target| target.server_name() == sender.server_name())
        || power_levels.is_some_and(|levels| levels.user_can_redact_event_of_other(sender))
}

fn redact_stored(
    candidate: UnappliedStateRedaction,
    rules: &RedactionRules,
) -> Option<(AnySyncStateEvent, serde_json::Value)> {
    let event = serde_json::json!({
        "type": candidate.event_type,
        "state_key": candidate.state_key,
        "event_id": candidate.event_id,
        "sender": candidate.sender,
        "origin_server_ts": candidate.origin_ts,
        "content": candidate.content?,
    });
    let event: CanonicalJsonObject = serde_json::from_value(event).ok()?;
    let because: CanonicalJsonObject = serde_json::from_value(candidate.redaction).ok()?;
    let redacted = redact(event, rules, Some(RedactedBecause::from_json(because))).ok()?;
    let value = serde_json::to_value(redacted).ok()?;
    let ev = serde_json::from_value::<AnySyncStateEvent>(value.clone()).ok()?;
    Some((ev, value))
}

async fn reconcile_room(
    ctx: &PersistContext,
    client: &Client,
    room_id: &OwnedRoomId,
    check_space_links: bool,
    after: &mut Option<RepairCursor>,
) {
    let mut missing = Vec::new();
    for event_type in STATE_TYPES {
        match cached_state(ctx, client, room_id, event_type).await {
            Cached::Redacted(ev, value) => apply(ctx, room_id, event_type, &ev, &value).await,
            Cached::Missing if SUPPLEMENTARY_STATE_TYPES.contains(event_type) => {
                missing.push(*event_type);
            }
            Cached::Missing | Cached::Unusable => {}
        }
    }
    // Singleton repairs run ahead of bounded space-link paging on every
    // pass. A singleton hint may arrive after this room's link cursor moved.
    if !missing.is_empty() {
        reconcile_stored_state(ctx, client, room_id, &missing, &mut None).await;
    }
    if check_space_links {
        reconcile_stored_state(ctx, client, room_id, SPACE_LINK_TYPES, after).await;
    } else {
        *after = None;
    }
}

/// Mirror one redacted form into the current tuple, if it still is current.
async fn apply(
    ctx: &PersistContext,
    room_id: &OwnedRoomId,
    event_type: &str,
    ev: &AnySyncStateEvent,
    value: &serde_json::Value,
) {
    let RoomStateRedaction::Redacted { event_id } = state_redaction_evidence(ev, value) else {
        return;
    };
    let state = RoomStateUpsert {
        account_id: ctx.account_id,
        room_id: room_id.as_str(),
        event_type,
        state_key: ev.state_key(),
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
            tracing::debug!(account_id = %ctx.account_id, room_id = %room_id, event_type, event_id = %ev.event_id(), "reconciled redacted room state");
            mark_room_state_changes(ctx, room_id, event_type, ev.state_key(), false);
        }
        Ok(false) => {}
        Err(_) => {
            tracing::warn!(account_id = %ctx.account_id, room_id = %room_id, event_type, "redacted state persistence failed; sweep will retry")
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
    fn redactions_apply_from_the_same_server_or_with_power_to_redact_others() {
        use matrix_sdk::ruma::events::room::power_levels::RoomPowerLevelsSource;
        use matrix_sdk::ruma::room_version_rules::AuthorizationRules;
        let redaction = |sender: &str| json!({"type": "m.room.redaction", "sender": sender});
        let levels = RoomPowerLevels::new(
            RoomPowerLevelsSource::Original(
                serde_json::from_value(json!({"redact": 50, "users": {
                    "@mod:other.example": 50, "@helper:other.example": 49}}))
                .unwrap(),
            ),
            &AuthorizationRules::V11,
            Vec::new(),
        );
        let target = "@admin:example.org";
        assert!(redaction_authorized(
            &redaction("@admin:example.org"),
            target,
            None
        ));
        assert!(redaction_authorized(
            &redaction("@other:example.org"),
            target,
            None
        ));
        assert!(!redaction_authorized(
            &redaction("@mod:other.example"),
            target,
            None
        ));
        assert!(redaction_authorized(
            &redaction("@mod:other.example"),
            target,
            Some(&levels)
        ));
        assert!(!redaction_authorized(
            &redaction("@helper:other.example"),
            target,
            Some(&levels)
        ));
        assert!(!redaction_authorized(
            &redaction("not a user"),
            target,
            Some(&levels)
        ));
        assert!(!redaction_authorized(&json!({}), target, Some(&levels)));
    }

    /// Repair announces `metadata` only if it covers every metadata type.
    #[test]
    fn repair_covers_every_metadata_type() {
        for event_type in axon_core::room_details::METADATA_STATE_TYPES {
            assert!(STATE_TYPES.contains(&event_type), "{event_type}");
        }
    }

    async fn repair_fixture() -> (
        PersistContext,
        Client,
        OwnedRoomId,
        mpsc::Receiver<OwnedRoomId>,
    ) {
        let store = axon_store::Store::connect(&std::env::var("DATABASE_URL").unwrap(), 5)
            .await
            .unwrap();
        let account = store
            .upsert_account(
                &format!("@supplementary-redaction-{}:localhost", Uuid::new_v4()),
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
        let room_id: OwnedRoomId = "!supplementary:localhost".parse().unwrap();
        let mut changes = StateChanges::default();
        changes.add_room(RoomInfo::new(&room_id, RoomState::Joined));
        client.state_store().save_changes(&changes).await.unwrap();
        let (state_redaction_tx, state_redaction_rx) = mpsc::channel(32);
        let (live_tx, _) = tokio::sync::broadcast::channel(8);
        let room_details =
            crate::room_details::RoomDetailsChanges::new(account.account_id, live_tx.clone());
        let ctx = PersistContext {
            store,
            account_id: account.account_id,
            live_tx,
            index: None,
            local_user_id: std::sync::Arc::from(account.user_id.as_str()),
            purge_on_leave: false,
            state_redaction_tx,
            room_details,
            gaps: None,
        };
        (ctx, client, room_id, state_redaction_rx)
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn stored_space_repair_resumes_past_a_timeout_and_unauthorized_prefix() {
        let (ctx, client, room, _) = repair_fixture().await;
        let last = STATE_REDACTION_PAGE_SIZE;
        let key = |i| format!("!child-{i:03}:localhost");
        for i in 0..=last {
            let target = format!("$child-{i}:localhost");
            ctx.store
                .upsert_room_state(&RoomStateUpsert {
                    account_id: ctx.account_id,
                    room_id: room.as_str(),
                    event_type: "m.space.child",
                    state_key: &key(i),
                    event_id: &target,
                    sender: "@alice:localhost",
                    origin_ts: 10,
                    content: Some(json!({"via": ["localhost"]})),
                })
                .await
                .unwrap();
            let event_id = format!("$redaction-{i}:localhost");
            let sender = if i == 0 || i == last {
                "@alice:localhost"
            } else {
                "@foreign:example.org"
            };
            let raw = json!({"type":"m.room.redaction","event_id":event_id,"sender":sender,
                "origin_server_ts":11,"redacts":target,"content":{"redacts":target}});
            ctx.store
                .upsert_event(&axon_store::NewEvent {
                    event_id: &event_id,
                    room_id: room.as_str(),
                    account_id: ctx.account_id,
                    sender,
                    origin_ts: 11,
                    event_type: "m.room.redaction",
                    content: Some(raw["content"].clone()),
                    raw_event: raw,
                    megolm_session_id: None,
                    redacts: Some(&target),
                    relates_to: None,
                    decrypted_body_text: None,
                })
                .await
                .unwrap();
        }
        let mut lock = ctx.store.pool().begin().await.unwrap();
        sqlx_core::query::query(
            "SELECT 1 FROM room_state WHERE account_id = $1 AND room_id = $2
                                AND event_type = 'm.space.child' AND state_key = $3 FOR UPDATE",
        )
        .bind(ctx.account_id)
        .bind(room.as_str())
        .bind(key(0))
        .execute(&mut *lock)
        .await
        .unwrap();
        let mut after = None;
        assert!(tokio::time::timeout(
            Duration::from_millis(250),
            reconcile_room(&ctx, &client, &room, true, &mut after)
        )
        .await
        .is_err());
        assert_eq!(after, Some(("m.space.child".into(), key(0))));
        lock.rollback().await.unwrap();

        reconcile_room(&ctx, &client, &room, true, &mut after).await;
        assert_eq!(after, Some(("m.space.child".into(), key(last))));
        for (i, redacted) in [(0, None), (1, None), (last, Some(true))] {
            let state = ctx
                .store
                .room_state(ctx.account_id, room.as_str(), "m.space.child", &key(i))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(state.redacted, redacted, "link {i}");
        }
        assert_eq!(
            ctx.room_details.take_pending()[room.as_str()],
            [axon_core::RoomDetailSource::SpaceGraph].into()
        );

        reconcile_room(&ctx, &client, &room, true, &mut after).await;
        assert!(after.is_none());
        reconcile_room(&ctx, &client, &room, true, &mut after).await;
        assert_eq!(
            ctx.store
                .room_state(ctx.account_id, room.as_str(), "m.space.child", &key(0))
                .await
                .unwrap()
                .unwrap()
                .redacted,
            Some(true)
        );
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn singleton_hint_repairs_state_ahead_of_a_paused_space_link_cursor() {
        let (ctx, client, room, mut hints) = repair_fixture().await;
        let event_id = "$stored-acl:localhost";
        let redaction_id = "$acl-redaction:localhost";
        let event_type = "m.room.server_acl";
        let state = RoomStateUpsert {
            account_id: ctx.account_id,
            room_id: room.as_str(),
            event_type,
            state_key: "",
            event_id,
            sender: "@alice:localhost",
            origin_ts: 10,
            content: Some(json!({"allow":["*"],"deny":["blocked.example"]})),
        };
        ctx.store.upsert_room_state(&state).await.unwrap();
        assert!(matches!(
            cached_state(&ctx, &client, &room, event_type).await,
            Cached::Missing
        ));

        let content = json!({"redacts": event_id});
        ctx.store
            .upsert_event(&axon_store::NewEvent {
                event_id: redaction_id,
                room_id: room.as_str(),
                account_id: ctx.account_id,
                sender: state.sender,
                origin_ts: 11,
                event_type: "m.room.redaction",
                content: Some(content.clone()),
                raw_event: json!({"type":"m.room.redaction","event_id":redaction_id,
                "sender":state.sender,"origin_server_ts":11,"redacts":event_id,"content":content}),
                megolm_session_id: None,
                redacts: Some(event_id),
                relates_to: None,
                decrypted_body_text: None,
            })
            .await
            .unwrap();
        queue_redaction_hint(&ctx, &room, event_id).await;
        assert_eq!(hints.try_recv().unwrap(), room);
        let mut after = Some(("m.space.child".to_owned(), "!cursor:localhost".to_owned()));
        reconcile_room(&ctx, &client, &room, true, &mut after).await;

        let row = ctx
            .store
            .room_state(ctx.account_id, room.as_str(), event_type, "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.redacted, Some(true));
        assert_eq!(row.content, Some(json!({})));
        assert_eq!(
            ctx.room_details.take_pending()[room.as_str()],
            [axon_core::RoomDetailSource::Metadata].into()
        );
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn graph_invalidation_uses_stored_space_state_when_the_sdk_room_is_absent() {
        let (ctx, client, room, _) = repair_fixture().await;
        assert!(client.get_room(&room).is_none());
        let persist = |kind, key, event_id, content| {
            let value = json!({"type":kind,"state_key":key,"event_id":event_id,
                "sender":"@alice:localhost","origin_server_ts":10,"content":content});
            let ev: AnySyncStateEvent = serde_json::from_value(value.clone()).unwrap();
            let (ctx, client, room) = (&ctx, &client, &room);
            async move {
                crate::engine::persist_room_state_value(ev, &value, room, client, ctx).await;
            }
        };
        use axon_core::RoomDetailSource::{Metadata, SpaceGraph};
        // Creation state can arrive while a membership write is in flight.
        persist(
            "m.room.member",
            "@alice:localhost",
            "$early-member",
            json!({"membership":"join"}),
        )
        .await;
        assert_eq!(
            ctx.room_details.take_pending()[room.as_str()],
            [SpaceGraph].into()
        );
        persist(
            "m.room.create",
            "",
            "$space",
            json!({"type":"m.space","room_version":"11"}),
        )
        .await;
        ctx.room_details.take_pending();
        let levels = json!({"users":{"@alice:localhost":0},"state_default":50});
        persist("m.room.power_levels", "", "$demotion", levels.clone()).await;
        assert_eq!(
            ctx.room_details.take_pending()[room.as_str()],
            [Metadata, SpaceGraph].into()
        );
        persist("m.room.power_levels", "", "$demotion", levels).await;
        assert!(ctx.room_details.take_pending().is_empty());
        persist(
            "m.room.member",
            "@alice:localhost",
            "$departure",
            json!({"membership":"leave"}),
        )
        .await;
        assert_eq!(
            ctx.room_details.take_pending()[room.as_str()],
            [SpaceGraph].into()
        );
    }

    /// Guest access and server ACL can reach Axon only through the
    /// supplementary connection, so the SDK cache never holds them. A synced
    /// redaction must still prune them and announce the change (ADR 0126).
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn supplementary_only_state_is_redacted_without_an_sdk_copy() {
        let (ctx, client, room_id, mut state_redaction_rx) = repair_fixture().await;
        let (store, room_details) = (&ctx.store, &ctx.room_details);
        // The supplementary transport's projection; the SDK never sees these.
        for (event_type, state_key, event_id, content) in [
            (
                "m.room.guest_access",
                "",
                "$guest:localhost",
                json!({"guest_access": "can_join"}),
            ),
            (
                "m.room.server_acl",
                "",
                "$acl:localhost",
                json!({"allow": ["*"], "deny": ["evil.example"], "allow_ip_literals": false}),
            ),
            (
                "m.space.child",
                "!child:localhost",
                "$child:localhost",
                json!({"via":["localhost"]}),
            ),
            (
                "m.space.parent",
                "!parent:localhost",
                "$parent:localhost",
                json!({"via":["localhost"],"canonical":true}),
            ),
        ] {
            let value = json!({"type": event_type, "state_key": state_key, "event_id": event_id,
                "sender": "@alice:localhost", "origin_server_ts": 10, "content": content});
            let ev: AnySyncStateEvent = serde_json::from_value(value.clone()).unwrap();
            crate::engine::persist_room_state_value(ev, &value, &room_id, &client, &ctx).await;
            assert!(client
                .state_store()
                .get_state_event(&room_id, event_type.into(), state_key)
                .await
                .unwrap()
                .is_none());
        }
        let sources = [
            axon_core::RoomDetailSource::Metadata,
            axon_core::RoomDetailSource::SpaceGraph,
        ];
        assert_eq!(
            room_details.take_pending()[room_id.as_str()],
            sources.into()
        );
        // Nothing to repair before a redaction is stored.
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
        assert!(room_details.take_pending().is_empty());
        // The primary connection persists the timeline redactions.
        for (redaction_id, target) in [
            ("$redact-guest:localhost", "$guest:localhost"),
            ("$redact-acl:localhost", "$acl:localhost"),
            ("$redact-child:localhost", "$child:localhost"),
            ("$redact-parent:localhost", "$parent:localhost"),
        ] {
            let raw = json!({"type": "m.room.redaction", "event_id": redaction_id,
                "sender": "@alice:localhost", "origin_server_ts": 11, "redacts": target,
                "content": {"redacts": target}});
            store
                .upsert_event(&axon_store::NewEvent {
                    event_id: redaction_id,
                    room_id: room_id.as_str(),
                    account_id: ctx.account_id,
                    sender: "@alice:localhost",
                    origin_ts: 11,
                    event_type: "m.room.redaction",
                    content: Some(raw["content"].clone()),
                    raw_event: raw,
                    megolm_session_id: None,
                    redacts: Some(target),
                    relates_to: None,
                    decrypted_body_text: None,
                })
                .await
                .unwrap();
            queue_redaction_hint(&ctx, &room_id, target).await;
            assert_eq!(state_redaction_rx.try_recv().unwrap(), room_id);
        }
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
        for (event_type, state_key, event_id, redaction_id) in [
            (
                "m.room.guest_access",
                "",
                "$guest:localhost",
                "$redact-guest:localhost",
            ),
            (
                "m.room.server_acl",
                "",
                "$acl:localhost",
                "$redact-acl:localhost",
            ),
            (
                "m.space.child",
                "!child:localhost",
                "$child:localhost",
                "$redact-child:localhost",
            ),
            (
                "m.space.parent",
                "!parent:localhost",
                "$parent:localhost",
                "$redact-parent:localhost",
            ),
        ] {
            let row = store
                .room_state(ctx.account_id, room_id.as_str(), event_type, state_key)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.event_id, event_id);
            assert_eq!(row.redacted, Some(true), "{event_type}");
            assert_eq!(row.redaction_event_id.as_deref(), Some(redaction_id));
            assert_eq!(row.content, Some(json!({})), "{event_type} is pruned");
        }
        assert_eq!(
            room_details.take_pending()[room_id.as_str()],
            sources.into()
        );
        // Applied once; the next sweep finds nothing left to apply.
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
        assert!(room_details.take_pending().is_empty());
        // A replacement is not touched by its predecessor's redaction.
        let value = json!({"type": "m.room.server_acl", "state_key": "", "event_id": "$acl2:localhost",
            "sender": "@alice:localhost", "origin_server_ts": 20,
            "content": {"allow": ["*"], "deny": []}});
        let ev: AnySyncStateEvent = serde_json::from_value(value.clone()).unwrap();
        crate::engine::persist_room_state_value(ev, &value, &room_id, &client, &ctx).await;
        room_details.take_pending();
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
        let row = store
            .room_state(ctx.account_id, room_id.as_str(), "m.room.server_acl", "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.event_id.as_str(), row.redacted, row.content),
            (
                "$acl2:localhost",
                None,
                Some(json!({"allow": ["*"], "deny": []}))
            )
        );
        assert!(room_details.take_pending().is_empty());
        // A homeserver delivers redactions it has not applied. One from
        // another server, with no power to redact others' events, is not
        // applied here either.
        let raw = json!({"type": "m.room.redaction", "event_id": "$bogus:elsewhere.example",
            "sender": "@evil:elsewhere.example", "origin_server_ts": 21,
            "redacts": "$acl2:localhost", "content": {"redacts": "$acl2:localhost"}});
        store
            .upsert_event(&axon_store::NewEvent {
                event_id: "$bogus:elsewhere.example",
                room_id: room_id.as_str(),
                account_id: ctx.account_id,
                sender: "@evil:elsewhere.example",
                origin_ts: 21,
                event_type: "m.room.redaction",
                content: Some(raw["content"].clone()),
                raw_event: raw,
                megolm_session_id: None,
                redacts: Some("$acl2:localhost"),
                relates_to: None,
                decrypted_body_text: None,
            })
            .await
            .unwrap();
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
        let row = store
            .room_state(ctx.account_id, room_id.as_str(), "m.room.server_acl", "")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.redacted, row.content),
            (None, Some(json!({"allow": ["*"], "deny": []}))),
            "an unauthorized redaction was applied"
        );
        assert!(room_details.take_pending().is_empty());
        store.delete_account_row(ctx.account_id).await.unwrap();
    }

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
        let room_details =
            crate::room_details::RoomDetailsChanges::new(account.account_id, live_tx.clone());
        let ctx = PersistContext {
            store: store.clone(),
            account_id: account.account_id,
            live_tx,
            index: None,
            local_user_id: std::sync::Arc::from(account.user_id.as_str()),
            purge_on_leave: false,
            state_redaction_tx,
            room_details: room_details.clone(),
            gaps: None,
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
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
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
        assert!(room_details.take_pending().is_empty());
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
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
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
        // Applied redaction evidence is a metadata change (ADR 0126).
        assert_eq!(
            room_details.take_pending()[room_id.as_str()]
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [
                axon_core::RoomDetailSource::Metadata,
                axon_core::RoomDetailSource::SpaceGraph
            ]
        );
        // Repeated repair is idempotent; it cannot recreate purged state.
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
        assert!(room_details.take_pending().is_empty());
        // The SDK cache still holds the old redacted event. Even though its
        // content differs, it must not touch a newer Axon current-state event.
        let replacement = RoomStateUpsert {
            event_id: "$replacement:localhost",
            origin_ts: 20,
            content: Some(json!({"ban":80,"invite":40})),
            ..state
        };
        store.upsert_room_state(&replacement).await.unwrap();
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
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
        reconcile_room(&ctx, &client, &room_id, true, &mut None).await;
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
