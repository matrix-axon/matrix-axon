use super::*;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use matrix_sdk::{
    ruma::{
        api::client::keys::get_keys, device_id,
        events::room::encryption::RoomEncryptionEventContent, room_id, serde::Raw, user_id,
        TransactionId,
    },
    RoomInfo, RoomState, StateChanges,
};
use matrix_sdk_base::crypto::{olm::Account as CryptoAccount, OlmMachine, TrustRequirement};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};

const DEVICE: &str = "AXON";
const PEER: &str = "@peer:localhost";
const ROOM_ID: &str = "!encrypted:localhost";
const NEWLY_ENCRYPTED: &str = "!newly-encrypted:localhost";
const EVENT_ID: &str = "$decrypted:localhost";

async fn room_fixture(client: &Client) {
    let mut changes = StateChanges::default();
    for (room_id, encrypted) in [
        (room_id!("!encrypted:localhost"), true),
        (room_id!("!plain:localhost"), false),
        (room_id!("!newly-encrypted:localhost"), false),
    ] {
        let mut room = RoomInfo::new(room_id, RoomState::Joined);
        if encrypted {
            room.set_encryption_event(Some(
                RoomEncryptionEventContent::with_recommended_defaults().into(),
            ));
        }
        if room_id.as_str() != NEWLY_ENCRYPTED {
            room.mark_encryption_state_synced();
        }
        room.mark_members_synced();
        changes.add_room(room);
        for member in [PEER, "@no-device:localhost"] {
            let event = Raw::from_json_string(
                json!({
                    "type": "m.room.member", "state_key": member, "sender": member,
                    "event_id": format!("$join-{member}"), "origin_server_ts": 1,
                    "content": {"membership": "join"}
                })
                .to_string(),
            )
            .unwrap();
            changes.add_state_event(room_id, event.deserialize().unwrap(), event);
        }
    }
    client.state_store().save_changes(&changes).await.unwrap();
}

/// The other session signs our real device key. As after device-only SAS,
/// only the public identity and device signature arrive at Axon.
async fn cross_sign_from_another_session(client: &Client, keys: &Mutex<Value>) {
    let own = client.user_id().unwrap();
    let device = client.device_id().unwrap();
    let signer = OlmMachine::new(own, device_id!("OTHER")).await;
    let signing = signer
        .bootstrap_cross_signing(true)
        .await
        .unwrap()
        .upload_signing_keys_req;
    let own_device = client.encryption().get_own_device().await.unwrap().unwrap();
    let mut public_keys = json!(own_device.as_device_keys());
    let mut query = get_keys::v3::Response::new();
    query.device_keys = serde_json::from_value(json!({own: {device: public_keys}})).unwrap();
    signer
        .mark_request_as_sent(&TransactionId::new(), &query)
        .await
        .unwrap();
    let device_to_sign = signer.get_device(own, device, None).await.unwrap().unwrap();
    let signatures = json!(device_to_sign.verify().await.unwrap().signed_keys);
    public_keys["signatures"][own.as_str()]
        .as_object_mut()
        .unwrap()
        .extend(
            signatures[own.as_str()][device.as_str()]["signatures"][own.as_str()]
                .as_object()
                .unwrap()
                .clone(),
        );

    {
        let mut keys = keys.lock().unwrap();
        keys["master_keys"] = json!({own: signing.master_key});
        keys["self_signing_keys"] = json!({own: signing.self_signing_key});
        keys["user_signing_keys"] = json!({own: signing.user_signing_key});
        keys["device_keys"][own.as_str()] = json!({device: public_keys});
    }
    let identity = client
        .encryption()
        .request_user_identity(own)
        .await
        .unwrap()
        .unwrap();
    assert!(!identity.is_verified());
    assert!(derive_verified(client).await);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn insecure_devices_requires_current_impact_and_preserves_decrypted_history() {
    let fixture = tempfile::tempdir().unwrap();
    let lc = lifecycle_with(SyncConfig {
        data_dir: fixture.path().to_owned(),
        store_key: Some("test-key".to_owned()),
        ..SyncConfig::default()
    })
    .await;
    let own = format!("@insecure-devices-{}:localhost", Uuid::new_v4().simple());
    let keys = Arc::new(Mutex::new(json!({"device_keys": {}, "failures": {}})));
    let newly_encrypted = Arc::new(AtomicBool::new(false));
    let remotely_encrypted = newly_encrypted.clone();
    let room_encryption = get(move |Path(room_id): Path<String>| {
        let encrypted = room_id == NEWLY_ENCRYPTED && remotely_encrypted.load(Ordering::SeqCst);
        async move {
            if encrypted {
                Json(RoomEncryptionEventContent::with_recommended_defaults()).into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"errcode": "M_NOT_FOUND", "error": "no encryption"})),
                )
                    .into_response()
            }
        }
    });
    let hs = Router::new()
        .route(
            "/_matrix/client/v3/rooms/{room_id}/state/m.room.encryption",
            room_encryption.clone(),
        )
        .route(
            "/_matrix/client/v3/rooms/{room_id}/state/m.room.encryption/",
            room_encryption,
        )
        .route(
            "/_matrix/client/versions",
            get(|| async { Json(json!({"versions": ["v1.11"]})) }),
        )
        .route(
            "/_matrix/client/v3/keys/query",
            post(|State(keys): State<Arc<Mutex<Value>>>| async move {
                Json(keys.lock().unwrap().clone())
            }),
        )
        .route(
            "/_matrix/client/v3/keys/upload",
            post(
                |State(keys): State<Arc<Mutex<Value>>>, Json(upload): Json<Value>| async move {
                    if let Some(user) = upload["device_keys"]["user_id"].as_str() {
                        keys.lock().unwrap()["device_keys"][user] =
                            json!({DEVICE: upload["device_keys"]});
                    }
                    Json(json!({"one_time_key_counts": {"signed_curve25519": 50}}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/keys/device_signing/upload",
            post(
                |State(keys): State<Arc<Mutex<Value>>>, Json(signing): Json<Value>| async move {
                    let user = signing["master_key"]["user_id"].as_str().unwrap();
                    let mut response = keys.lock().unwrap();
                    response["master_keys"] = json!({user: signing["master_key"]});
                    response["self_signing_keys"] = json!({user: signing["self_signing_key"]});
                    response["user_signing_keys"] = json!({user: signing["user_signing_key"]});
                    Json(json!({}))
                },
            ),
        )
        .route(
            "/_matrix/client/v3/keys/signatures/upload",
            post(
                |State(keys): State<Arc<Mutex<Value>>>, Json(signatures): Json<Value>| async move {
                    let mut response = keys.lock().unwrap();
                    for (user, signed) in signatures.as_object().unwrap() {
                        if let Some(device_signatures) =
                            signed[DEVICE]["signatures"][user].as_object()
                        {
                            response["device_keys"][user][DEVICE]["signatures"][user]
                                .as_object_mut()
                                .unwrap()
                                .extend(device_signatures.clone());
                        }
                    }
                    Json(json!({"failures": {}}))
                },
            ),
        )
        .with_state(keys.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let homeserver = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, hs).await.unwrap() });

    let account = lc.store.upsert_account(&own, &homeserver).await.unwrap();
    let account_id = account.account_id;
    assert!(!account.exclude_insecure_devices);
    lc.store
        .set_account_matrix_session(account_id, DEVICE, "fixture", "test-key")
        .await
        .unwrap();
    let client = lc.manager.get_or_connect(account_id).await.unwrap();
    room_fixture(&client).await;
    tokio::time::timeout(
        Duration::from_secs(15),
        client.encryption().wait_for_e2ee_initialization_tasks(),
    )
    .await
    .expect("initial SDK setup finishes before the fixture evicts its client");
    drop(client);
    lc.manager.evict(account_id).await;
    let client = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match lc.manager.get_or_connect(account_id).await {
                Ok(client) => break client,
                Err(GatewayError::NotConnected(_)) => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("{error}"),
            }
        }
    })
    .await
    .expect("the fixture client can reopen after the original closes");
    assert!(matches!(
        lc.insecure_devices_impact(account_id).await,
        Err(InsecureDevicesError::ImpactUnavailable)
    ));
    let room = client
        .get_room(room_id!("!newly-encrypted:localhost"))
        .unwrap();
    let base_room: &matrix_sdk_base::Room = &room;
    base_room
        .update_and_save_room_info(|mut info| {
            info.mark_encryption_state_synced();
            (info, matrix_sdk_base::RoomInfoNotableUpdateReasons::empty())
        })
        .await
        .unwrap();
    drop(room);
    drop(client);

    let initial = lc.insecure_devices_impact(account_id).await.unwrap();
    assert!(initial.rooms.is_empty());
    assert!(matches!(
        lc.set_exclude_insecure_devices(account_id, true, Some(&initial.acknowledgement_token))
            .await,
        Err(InsecureDevicesError::OwnDeviceNotCrossSigned)
    ));
    let client = lc.manager.get_or_connect(account_id).await.unwrap();
    cross_sign_from_another_session(&client, &keys).await;
    drop(client);
    assert!(matches!(
        lc.set_exclude_insecure_devices(account_id, true, Some(&initial.acknowledgement_token))
            .await,
        Err(InsecureDevicesError::OwnIdentityNotVerified)
    ));

    let client = lc.manager.get_or_connect(account_id).await.unwrap();
    let own_identity = client
        .encryption()
        .get_user_identity(client.user_id().unwrap())
        .await
        .unwrap()
        .unwrap();
    own_identity.verify().await.unwrap();
    assert!(own_identity.is_verified());
    drop(own_identity);

    let peer = CryptoAccount::with_device_id(user_id!("@peer:localhost"), device_id!("PEER"));
    keys.lock().unwrap()["device_keys"][PEER] = json!({"PEER": peer.device_keys()});
    client
        .encryption()
        .request_user_identity(user_id!("@peer:localhost"))
        .await
        .unwrap();
    drop(client);
    let impact = lc.insecure_devices_impact(account_id).await.unwrap();
    assert_eq!(impact.rooms.len(), 1);
    assert_eq!(impact.rooms[0].room_id, ROOM_ID);
    assert_eq!(impact.rooms[0].member_user_ids, [PEER]);
    for token in [None, Some(initial.acknowledgement_token.as_str())] {
        let error = lc
            .set_exclude_insecure_devices(account_id, true, token)
            .await
            .unwrap_err();
        let InsecureDevicesError::AcknowledgementRequired(current) = error else {
            panic!("{error}");
        };
        assert_eq!(current, impact);
    }

    // A normal room request holds the old client. The PUT must wait for it
    // and recompute the impact after it drains.
    let room = lc
        .manager
        .get_or_connect(account_id)
        .await
        .unwrap()
        .room(room_id!("!plain:localhost"))
        .unwrap();
    let stale_token = impact.acknowledgement_token;
    let mut enable = tokio::spawn({
        let lc = lc.clone();
        async move {
            lc.set_exclude_insecure_devices(account_id, true, Some(&stale_token))
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while lc.manager.cached(account_id).is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("enable queued behind the room request");
    assert!(tokio::time::timeout(Duration::from_millis(50), &mut enable)
        .await
        .is_err());
    newly_encrypted.store(true, Ordering::SeqCst);
    drop(room);
    let error = enable.await.unwrap().unwrap_err();
    let InsecureDevicesError::AcknowledgementRequired(impact) = error else {
        panic!("{error}");
    };
    assert_eq!(
        impact
            .rooms
            .iter()
            .map(|room| room.room_id.as_str())
            .collect::<Vec<_>>(),
        [ROOM_ID, NEWLY_ENCRYPTED]
    );
    assert!(impact
        .rooms
        .iter()
        .all(|room| room.member_user_ids == [PEER]));

    let plaintext = json!({"msgtype": "m.text", "body": "decrypted before enabling"});
    lc.store
        .upsert_event(&axon_store::NewEvent {
            event_id: EVENT_ID,
            room_id: ROOM_ID,
            account_id,
            sender: PEER,
            origin_ts: 1,
            event_type: "m.room.message",
            content: Some(plaintext.clone()),
            raw_event: json!({"type": "m.room.message", "content": plaintext}),
            megolm_session_id: None,
            redacts: None,
            relates_to: None,
            decrypted_body_text: Some("decrypted before enabling"),
        })
        .await
        .unwrap();

    lc.set_exclude_insecure_devices(account_id, true, Some(&impact.acknowledgement_token))
        .await
        .unwrap();
    let restored = lc.manager.get_or_connect(account_id).await.unwrap();
    assert!(matches!(
        restored
            .decryption_settings()
            .sender_device_trust_requirement,
        TrustRequirement::CrossSignedOrLegacy
    ));
    assert_eq!(restored.device_id().unwrap(), DEVICE);
    assert!(derive_verified(&restored).await);
    drop(restored);
    let stored = lc
        .store
        .get_event(account_id, EVENT_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, Some(plaintext));
    assert!(
        lc.store
            .get_account(account_id)
            .await
            .unwrap()
            .unwrap()
            .exclude_insecure_devices
    );

    lc.set_exclude_insecure_devices(account_id, false, None)
        .await
        .unwrap();
    assert!(matches!(
        lc.manager
            .get_or_connect(account_id)
            .await
            .unwrap()
            .decryption_settings()
            .sender_device_trust_requirement,
        TrustRequirement::Untrusted
    ));
    assert!(
        !lc.store
            .get_account(account_id)
            .await
            .unwrap()
            .unwrap()
            .exclude_insecure_devices
    );

    lc.cancel.cancel();
    lc.reap_task(account_id).await.unwrap();
    lc.manager.evict(account_id).await;
    delete_account(&lc.store, account_id).await;
    server.abort();
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn insecure_devices_fresh_password_and_token_sessions_reset_policy() {
    let fixture = tempfile::tempdir().unwrap();
    let lc = lifecycle_with(SyncConfig {
        data_dir: fixture.path().to_owned(),
        store_key: Some("test-key".to_owned()),
        ..SyncConfig::default()
    })
    .await;
    let own = format!("@fresh-device-{}:localhost", Uuid::new_v4().simple());
    let session = json!({"user_id": own, "device_id": DEVICE, "access_token": "fixture"});
    let hs = Router::new()
        .route(
            "/_matrix/client/versions",
            get(|| async { Json(json!({"versions": ["v1.11"], "unstable_features": {}})) }),
        )
        .route(
            "/_matrix/client/v3/login",
            post({
                let session = session.clone();
                move || async move { Json(session) }
            }),
        )
        .route(
            "/_matrix/client/v3/account/whoami",
            get({
                let own = own.clone();
                move || async move { Json(json!({"user_id": own, "device_id": DEVICE})) }
            }),
        )
        .route(
            "/_matrix/client/v3/logout",
            post(|| async { Json(json!({})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let homeserver = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, hs).await.unwrap() });
    let account_id = lc
        .store
        .upsert_account(&own, &homeserver)
        .await
        .unwrap()
        .account_id;

    for password_login in [true, false] {
        lc.store
            .set_exclude_insecure_devices(account_id, true)
            .await
            .unwrap();
        lc.logout(account_id).await.unwrap();
        let active_id = if password_login {
            lc.login(Some(&homeserver), &own, "fixture-password")
                .await
                .unwrap()
        } else {
            // Even an import with the same upstream device ID replaces Axon's
            // crypto store, so it starts with the default policy.
            lc.import_token(&homeserver, &own, "fixture", DEVICE)
                .await
                .unwrap()
        };
        assert_eq!(active_id, account_id);
        let active = lc.store.get_account(active_id).await.unwrap().unwrap();
        assert_eq!(active.state, AccountState::Active);
        assert!(!active.exclude_insecure_devices);
        let client = lc.manager.get_or_connect(active_id).await.unwrap();
        assert_eq!(client.device_id().unwrap(), DEVICE);
        assert!(matches!(
            client.decryption_settings().sender_device_trust_requirement,
            TrustRequirement::Untrusted
        ));
        tokio::time::timeout(
            Duration::from_secs(10),
            client.encryption().wait_for_e2ee_initialization_tasks(),
        )
        .await
        .unwrap();
    }
    lc.logout(account_id).await.unwrap();
    delete_account(&lc.store, account_id).await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Postgres"]
async fn insecure_devices_failed_drain_recovers_sync_when_the_old_task_exits() {
    use crate::engine::AccountTask;

    for logout_while_draining in [false, true] {
        let lc = lifecycle().await;
        let own = format!("@failed-drain-{}:localhost", Uuid::new_v4().simple());
        let account_id = lc
            .store
            .upsert_account(&own, "http://127.0.0.1:9")
            .await
            .unwrap()
            .account_id;
        lc.store
            .set_exclude_insecure_devices(account_id, true)
            .await
            .unwrap();

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let old = AccountTask::spawn(&lc.tracker, CancellationToken::new(), async move {
            started_tx.send(()).unwrap();
            // No await point until released, so abort cannot stop this run.
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        let old_id = old.handle.id();
        lc.tasks.lock().unwrap().insert(account_id, old);
        started_rx.await.unwrap();

        assert!(matches!(
            lc.set_exclude_insecure_devices(account_id, false, None).await,
            Err(InsecureDevicesError::Lifecycle(LifecycleError::Draining(id))) if id == account_id
        ));
        assert_eq!(lc.tasks.lock().unwrap()[&account_id].handle.id(), old_id);
        assert!(
            lc.store
                .get_account(account_id)
                .await
                .unwrap()
                .unwrap()
                .exclude_insecure_devices
        );

        if logout_while_draining {
            assert!(
                matches!(lc.logout(account_id).await, Err(LifecycleError::Draining(id)) if id == account_id)
            );
        }
        release.send(()).unwrap();

        if logout_while_draining {
            lc.tracker.close();
            tokio::time::timeout(Duration::from_secs(5), lc.tracker.wait())
                .await
                .unwrap();
            assert_eq!(lc.tasks.lock().unwrap()[&account_id].handle.id(), old_id);
            assert_eq!(
                lc.store
                    .get_account(account_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                AccountState::Deactivated
            );
        } else {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if lc
                        .tasks
                        .lock()
                        .unwrap()
                        .get(&account_id)
                        .is_some_and(|task| {
                            task.handle.id() != old_id && !task.cancel.is_cancelled()
                        })
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("a fresh supervisor should take over after the old run exits");
            assert!(
                lc.store
                    .get_account(account_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .exclude_insecure_devices
            );
        }

        lc.logout(account_id).await.unwrap();
        delete_account(&lc.store, account_id).await;
    }
}
