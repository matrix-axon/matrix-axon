use super::*;
use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use matrix_sdk::{
    ruma::{
        device_id, events::room::encryption::RoomEncryptionEventContent, room_id, serde::Raw,
        user_id,
    },
    RoomInfo, RoomState, StateChanges,
};
use matrix_sdk_base::crypto::{olm::Account as CryptoAccount, TrustRequirement};
use serde_json::{json, Value};

const DEVICE: &str = "AXON";
const PEER: &str = "@peer:localhost";
const ROOM_ID: &str = "!encrypted:localhost";
const EVENT_ID: &str = "$decrypted:localhost";

async fn room_fixture(client: &Client) {
    let mut changes = StateChanges::default();
    for (room_id, encrypted) in [
        (room_id!("!encrypted:localhost"), true),
        (room_id!("!plain:localhost"), false),
    ] {
        let mut room = RoomInfo::new(room_id, RoomState::Joined);
        if encrypted {
            room.set_encryption_event(Some(
                RoomEncryptionEventContent::with_recommended_defaults().into(),
            ));
        }
        room.mark_encryption_state_synced();
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
    let hs = Router::new()
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
                        let current = response["device_keys"][user][DEVICE]["signatures"][user]
                            .as_object_mut()
                            .unwrap();
                        current.extend(
                            signed[DEVICE]["signatures"][user]
                                .as_object()
                                .unwrap()
                                .clone(),
                        );
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
    drop(client);
    lc.manager.evict(account_id).await;
    let client = lc.manager.get_or_connect(account_id).await.unwrap();

    let initial = lc.insecure_devices_impact(account_id).await.unwrap();
    assert!(initial.rooms.is_empty());
    assert!(matches!(
        lc.set_exclude_insecure_devices(account_id, true, Some(&initial.acknowledgement_token))
            .await,
        Err(InsecureDevicesError::OwnDeviceNotCrossSigned)
    ));
    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .unwrap();
    let own_id = client.user_id().unwrap();
    client
        .encryption()
        .request_user_identity(own_id)
        .await
        .unwrap();
    assert!(derive_verified(&client).await);

    let peer = CryptoAccount::with_device_id(user_id!("@peer:localhost"), device_id!("PEER"));
    keys.lock().unwrap()["device_keys"][PEER] = json!({"PEER": peer.device_keys()});
    client
        .encryption()
        .request_user_identity(user_id!("@peer:localhost"))
        .await
        .unwrap();
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
    drop(client);

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
