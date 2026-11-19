use super::*;
use axum::{extract::State, routing::post, Json, Router};
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    ruma::{
        api::MatrixVersion, device_id, events::room::encryption::RoomEncryptionEventContent,
        room_id, serde::Raw,
    },
    store::RoomLoadSettings,
    RoomInfo, SessionMeta, SessionTokens, StateChanges,
};
use matrix_sdk_base::crypto::{IdentityState, OlmMachine, UserIdentity as CryptoUserIdentity};
use serde_json::{json, Value};

const OWN: &str = "@alice:localhost";
const PEER: &str = "@bob:localhost";

fn rooms() -> [&'static RoomId; 2] {
    [room_id!("!a:localhost"), room_id!("!b:localhost")]
}

async fn publish_identity(machine: &OlmMachine, response: &Mutex<Value>) {
    let signing = machine
        .bootstrap_cross_signing(true)
        .await
        .unwrap()
        .upload_signing_keys_req;
    *response.lock().unwrap() = json!({
        "master_keys": {PEER: signing.master_key},
        "self_signing_keys": {PEER: signing.self_signing_key},
        "device_keys": {}, "failures": {},
    });
}

async fn wait_for_warnings(changes: &RoomIdentityChanges, account_id: Uuid, expected: &Warnings) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while rooms()
            .iter()
            .any(|id| changes.warnings(account_id, id).as_ref() != Some(expected))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both room subscriptions reach the expected warning state");
}

#[tokio::test]
async fn sdk_identity_warnings_clear_in_every_room_after_acknowledgement() {
    let own_id = <&UserId>::try_from(OWN).unwrap();
    let peer_id = <&UserId>::try_from(PEER).unwrap();
    let keys = Arc::new(Mutex::new(Value::Null));
    let server = Router::new()
        .route(
            "/_matrix/client/v3/keys/query",
            post(|State(keys): State<Arc<Mutex<Value>>>| async move {
                Json(keys.lock().unwrap().clone())
            }),
        )
        .route(
            "/_matrix/client/v3/keys/upload",
            post(|| async { Json(json!({"one_time_key_counts": {}})) }),
        )
        .route(
            "/_matrix/client/v3/keys/device_signing/upload",
            post(|| async { Json(json!({})) }),
        )
        .route(
            "/_matrix/client/v3/keys/signatures/upload",
            post(|| async { Json(json!({})) }),
        )
        .with_state(keys.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });

    let cache = tempfile::tempdir().unwrap();
    let build = || {
        Client::builder()
            .homeserver_url(format!("http://{address}"))
            .server_versions([MatrixVersion::V1_11])
            .sqlite_store_with_config_and_cache_path(
                matrix_sdk::SqliteStoreConfig::new(cache.path()),
                None::<&std::path::Path>,
            )
            .build()
    };
    let client = build().await.unwrap();
    let mut state = StateChanges::default();
    for room_id in rooms() {
        let mut room = RoomInfo::new(room_id, RoomState::Joined);
        room.set_encryption_event(Some(
            RoomEncryptionEventContent::with_recommended_defaults().into(),
        ));
        room.mark_encryption_state_synced();
        room.mark_members_synced();
        state.add_room(room);
        let member: Raw<matrix_sdk::ruma::events::AnySyncStateEvent> = Raw::from_json_string(
            json!({
                "type": "m.room.member", "state_key": PEER, "sender": PEER,
                "event_id": "$join", "origin_server_ts": 1, "content": {"membership": "join"}
            })
            .to_string(),
        )
        .unwrap();
        state.add_state_event(room_id, member.deserialize().unwrap(), member);
    }
    client.state_store().save_changes(&state).await.unwrap();
    drop(client);

    let client = build().await.unwrap();
    client
        .matrix_auth()
        .restore_session(
            MatrixSession {
                meta: SessionMeta {
                    user_id: own_id.to_owned(),
                    device_id: "ALICE".into(),
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
    let peer = OlmMachine::new(peer_id, device_id!("BOB")).await;
    let encryption = client.encryption();
    publish_identity(&peer, &keys).await;
    assert!(encryption
        .request_user_identity(peer_id)
        .await
        .unwrap()
        .is_some());

    let account_id = Uuid::new_v4();
    let changes = RoomIdentityChanges::default();
    let (live, _receiver) = tokio::sync::broadcast::channel(8);
    let room_details = RoomDetailsChanges::new(account_id, live);
    let cancel = CancellationToken::new();
    let watcher = tokio::spawn(watch(
        client.clone(),
        account_id,
        changes.clone(),
        room_details.clone(),
        HomeserverGate::for_test(),
        cancel.clone(),
    ));
    wait_for_warnings(&changes, account_id, &Warnings::new()).await;
    assert_eq!(
        room_details.take_pending().len(),
        2,
        "both rooms became observable"
    );

    publish_identity(&peer, &keys).await;
    encryption.request_user_identity(peer_id).await.unwrap();
    let room = client.get_room(rooms()[0]).unwrap();
    let (_, warning) = read_identity(&room, peer_id).await.unwrap().unwrap();
    let older = warning.unwrap();
    let kind = older.kind;
    assert_eq!(kind, IdentityChangeKind::PinViolation);
    wait_for_warnings(
        &changes,
        account_id,
        &Warnings::from([(peer_id.to_owned(), older.clone())]),
    )
    .await;
    assert_eq!(
        room_details.take_pending().len(),
        2,
        "first rotation notified both rooms"
    );

    // A second rotation keeps the same kind but must change both snapshots
    // and emit a new frame, independently of the drained first rotation.
    publish_identity(&peer, &keys).await;
    encryption.request_user_identity(peer_id).await.unwrap();
    let (identity, warning) = read_identity(&room, peer_id).await.unwrap().unwrap();
    let warning = warning.unwrap();
    assert_eq!(warning.kind, kind);
    assert_ne!(older.fingerprint, warning.fingerprint);
    wait_for_warnings(
        &changes,
        account_id,
        &Warnings::from([(peer_id.to_owned(), warning.clone())]),
    )
    .await;
    assert_eq!(
        room_details.take_pending().len(),
        2,
        "second rotation notified both rooms"
    );
    assert!(matches!(
        resolve_current_warning(
            account_id,
            &identity,
            Some(&warning),
            &older.fingerprint,
            kind
        )
        .await,
        Err(RoomIdentityError::Conflict)
    ));
    assert!(matches!(
        resolve_current_warning(
            account_id,
            &identity,
            Some(&warning),
            &warning.fingerprint,
            IdentityChangeKind::VerificationViolation
        )
        .await,
        Err(RoomIdentityError::Conflict)
    ));

    resolve_current_warning(
        account_id,
        &identity,
        Some(&warning),
        &warning.fingerprint,
        kind,
    )
    .await
    .unwrap();
    wait_for_warnings(&changes, account_id, &Warnings::new()).await;
    let pending = room_details.take_pending();
    for room_id in rooms() {
        assert_eq!(
            pending[room_id.as_str()],
            [RoomDetailSource::IdentityChanges].into()
        );
    }
    // Have the SDK sign the current peer identity. The mock key query then
    // returns the signed master key, as a homeserver would after verification.
    encryption.bootstrap_cross_signing(None).await.unwrap();
    let CryptoUserIdentity::Own(own) = room.user_identity(own_id).await.unwrap() else {
        panic!("the account has an own identity");
    };
    own.verify().await.unwrap();
    let CryptoUserIdentity::Other(other) = room.user_identity(peer_id).await.unwrap() else {
        panic!("the peer has an other-user identity");
    };
    let signed = other.verify().await.unwrap();
    let (_, signed_master) = signed.signed_keys[peer_id].iter().next().unwrap();
    keys.lock().unwrap()["master_keys"][PEER] = serde_json::from_str(signed_master.get()).unwrap();
    assert!(encryption
        .request_user_identity(peer_id)
        .await
        .unwrap()
        .unwrap()
        .is_verified());

    publish_identity(&peer, &keys).await;
    encryption.request_user_identity(peer_id).await.unwrap();
    let (identity, warning) = read_identity(&room, peer_id).await.unwrap().unwrap();
    let warning = warning.unwrap();
    let kind = warning.kind;
    assert_eq!(kind, IdentityChangeKind::VerificationViolation);
    wait_for_warnings(
        &changes,
        account_id,
        &Warnings::from([(peer_id.to_owned(), warning.clone())]),
    )
    .await;
    assert!(matches!(
        resolve_current_warning(
            account_id,
            &identity,
            Some(&warning),
            &warning.fingerprint,
            IdentityChangeKind::PinViolation
        )
        .await,
        Err(RoomIdentityError::Conflict)
    ));
    assert_eq!(room_details.take_pending().len(), 2);
    resolve_current_warning(
        account_id,
        &identity,
        Some(&warning),
        &warning.fingerprint,
        kind,
    )
    .await
    .unwrap();
    wait_for_warnings(&changes, account_id, &Warnings::new()).await;
    let identity = room.user_identity(peer_id).await.unwrap();
    assert!(!identity.was_previously_verified());
    assert_eq!(room.state_of(&identity), IdentityState::Pinned);
    assert_eq!(room_details.take_pending().len(), 2);

    cancel.cancel();
    watcher.await.unwrap();
    for room_id in rooms() {
        assert!(changes.warnings(account_id, room_id).is_none());
    }
    publish_identity(&peer, &keys).await;
    encryption.request_user_identity(peer_id).await.unwrap();
    assert_failed_refresh_keeps_warning(&client, &room, peer_id).await;
    server.abort();
}

async fn assert_failed_refresh_keeps_warning(client: &Client, room: &Room, peer: &UserId) {
    let account_id = Uuid::new_v4();
    let changes = RoomIdentityChanges::default();
    let (live, _receiver) = tokio::sync::broadcast::channel(8);
    let details = RoomDetailsChanges::new(account_id, live);
    let gate = HomeserverGate::for_test();
    let watcher = AccountWatch {
        account_id,
        changes: changes.clone(),
        details: details.clone(),
        gate: gate.clone(),
        initializing: Arc::new(Semaphore::new(1)),
    };
    let (_, snapshot) = watcher.subscribe(room).await.unwrap();
    assert!(snapshot.contains_key(peer));
    watcher.publish(room, snapshot.clone());
    assert_eq!(details.take_pending().len(), 1);

    gate.open_for_test();
    room.update_room_info(|mut info| {
        info.mark_members_missing();
        (info, matrix_sdk_base::RoomInfoNotableUpdateReasons::empty())
    })
    .await;
    assert!(
        watcher.subscribe(room).await.is_none(),
        "gate refuses roster work"
    );
    assert_retry_preserves_snapshot(&watcher, room, &snapshot).await;

    room.update_room_info(|mut info| {
        info.mark_members_synced();
        (info, matrix_sdk_base::RoomInfoNotableUpdateReasons::empty())
    })
    .await;
    // The malformed event replaces the state row. The membership index
    // still contains the peer, so the SDK fails to decode the roster.
    let mut changes = StateChanges::default();
    changes.state.insert(room.room_id().to_owned(), [(
        matrix_sdk::ruma::events::StateEventType::RoomMember,
        [(peer.to_string(), Raw::from_json_string(json!({
            "type": "m.room.member", "state_key": peer, "sender": peer,
            "event_id": "$broken", "origin_server_ts": 1, "content": {"membership": null}
        }).to_string()).unwrap())].into(),
    )].into());
    client.state_store().save_changes(&changes).await.unwrap();
    assert!(room.members_no_sync(RoomMemberships::JOIN).await.is_err());
    assert!(
        watcher.subscribe(room).await.is_none(),
        "SDK stream success does not validate its snapshot"
    );
    assert_retry_preserves_snapshot(&watcher, room, &snapshot).await;

    let mut served = snapshot.clone();
    assert!(member_warning(room, peer).await.is_err());
    recheck_room_warnings(account_id, room, &mut served).await;
    assert_eq!(
        served, snapshot,
        "room-info recheck keeps the known entry when the roster errors"
    );
}

async fn assert_retry_preserves_snapshot(watcher: &AccountWatch, room: &Room, expected: &Warnings) {
    let refresh = Notify::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), watcher.follow(room, &refresh))
            .await
            .is_err()
    );
    assert_eq!(
        watcher
            .changes
            .warnings(watcher.account_id, room.room_id())
            .as_ref(),
        Some(expected)
    );
    assert!(
        watcher.details.take_pending().is_empty(),
        "retry does not announce a clear"
    );
}

#[tokio::test(start_paused = true)]
async fn room_info_recheck_preserves_unavailable_entries_and_applies_confirmed_results() {
    let warning = Warning {
        kind: IdentityChangeKind::PinViolation,
        fingerprint: "a".repeat(64),
    };
    let new_warning = Warning {
        fingerprint: "b".repeat(64),
        ..warning.clone()
    };
    let ids = ["@a:x", "@b:x", "@c:x", "@d:x", "@e:x"].map(|id| id.parse::<OwnedUserId>().unwrap());
    let mut warnings = Warnings::from_iter(ids.iter().cloned().map(|id| (id, warning.clone())));
    refresh_known_warnings(Uuid::nil(), rooms()[0], &mut warnings, |user| {
        let new_warning = new_warning.clone();
        async move {
            match user.as_str() {
                "@a:x" => Err(ObservationError::IdentityUnavailable),
                "@b:x" => Ok(None),
                "@c:x" => Ok(Some(new_warning)),
                "@d:x" => std::future::pending().await,
                _ => panic!("the room timeout bounds the whole read"),
            }
        }
    })
    .await;
    assert_eq!(
        warnings,
        Warnings::from([
            (ids[0].clone(), warning.clone()),
            (ids[2].clone(), new_warning),
            (ids[3].clone(), warning.clone()),
            (ids[4].clone(), warning),
        ])
    );
}

#[test]
fn warning_map_can_be_read_after_poison() {
    let changes = RoomIdentityChanges::default();
    let account = Uuid::new_v4();
    let room = rooms()[0];
    assert!(std::panic::catch_unwind(|| {
        let mut guard = changes.lock();
        guard
            .entry(account)
            .or_default()
            .insert(room.to_owned(), Warnings::new());
        panic!("poison the test cache");
    })
    .is_err());
    assert_eq!(changes.warnings(account, room), Some(Warnings::new()));
}
