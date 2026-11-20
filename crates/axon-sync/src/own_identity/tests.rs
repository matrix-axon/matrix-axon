use std::panic::{resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use axum::{extract::State, routing::post, Json, Router};
use futures_util::FutureExt;
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    ruma::{api::client::keys::get_keys, api::MatrixVersion, device_id, TransactionId, UserId},
    store::RoomLoadSettings,
    SessionMeta, SessionTokens,
};
use matrix_sdk_base::crypto::{types::requests::UploadSigningKeysRequest, OlmMachine};
use serde_json::{json, Value};

use super::*;

async fn expect_warning(
    receiver: &mut broadcast::Receiver<LiveFrame>,
    account_id: Uuid,
    warning: bool,
    replaced: bool,
) {
    let frame = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("live identity update")
        .unwrap();
    let LiveFrame::OwnIdentityChanged(frame) = frame else {
        panic!("unexpected live frame");
    };
    assert_eq!(frame.account_id, account_id);
    assert_eq!(frame.own_identity_changed, warning);
    assert_eq!(frame.identity_replaced, replaced);
}

fn key_query(own: &UserId, signing: &UploadSigningKeysRequest, devices: Value) -> Value {
    json!({
        "master_keys": {own: signing.master_key},
        "self_signing_keys": {own: signing.self_signing_key},
        "user_signing_keys": {own: signing.user_signing_key},
        "device_keys": devices,
        "failures": {},
    })
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn sdk_identity_rotation_and_reverification_reach_the_account_and_live_bus() {
    let url = std::env::var("DATABASE_URL").expect("test database");
    let store = Store::connect(&url, 5)
        .await
        .expect("connect and migrate test DB");
    let own = UserId::parse(format!("@own-{}:localhost", Uuid::new_v4().simple())).unwrap();
    let device = device_id!("AXON");
    let signer = OlmMachine::new(&own, device_id!("OTHER")).await;
    let first = signer.bootstrap_cross_signing(true).await.unwrap();

    let keys = Arc::new(Mutex::new(key_query(
        &own,
        &first.upload_signing_keys_req,
        json!({}),
    )));
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
            "/_matrix/client/v3/keys/signatures/upload",
            post(|| async { Json(json!({"failures": {}})) }),
        )
        .with_state(keys.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let homeserver = format!("http://{address}");
    let server = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });

    let client = Client::builder()
        .homeserver_url(&homeserver)
        .server_versions([MatrixVersion::V1_11])
        .build()
        .await
        .unwrap();
    client
        .matrix_auth()
        .restore_session(
            MatrixSession {
                meta: SessionMeta {
                    user_id: own.clone(),
                    device_id: device.to_owned(),
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
    let encryption = client.encryption();
    tokio::time::timeout(
        Duration::from_secs(5),
        encryption.wait_for_e2ee_initialization_tasks(),
    )
    .await
    .expect("SDK encryption initialization");
    encryption
        .request_user_identity(&own)
        .await
        .unwrap()
        .unwrap()
        .verify()
        .await
        .unwrap();

    let account = store
        .upsert_account(own.as_str(), &homeserver)
        .await
        .unwrap();
    let result = AssertUnwindSafe(exercise_rotations(
        &client,
        &store,
        account.account_id,
        &keys,
        &signer,
    ))
    .catch_unwind()
    .await;
    server.abort();
    let cleanup = store.delete_account_row(account.account_id).await;
    if let Err(panic) = result {
        resume_unwind(panic);
    }
    cleanup.expect("delete fixture account");
}

async fn exercise_rotations(
    client: &Client,
    store: &Store,
    id: Uuid,
    keys: &Mutex<Value>,
    signer: &OlmMachine,
) {
    let own = client.user_id().unwrap();
    let device = client.device_id().unwrap();
    let encryption = client.encryption();
    let rotate = || async {
        let signing = signer
            .bootstrap_cross_signing(true)
            .await
            .unwrap()
            .upload_signing_keys_req;
        *keys.lock().unwrap() = key_query(own, &signing, json!({}));
        encryption
            .request_user_identity(own)
            .await
            .unwrap()
            .unwrap()
    };

    // Simulate an upgrade after the SDK saw a replacement. Axon still has
    // only its legacy verified flag, while the SDK remembers the rotation.
    store.set_account_verified(id, true).await.unwrap();
    assert!(rotate().await.has_verification_violation());
    let (live, mut receiver) = broadcast::channel(8);
    // Before the SDK read completes, the server can store device state only.
    // The first complete observation must still recognize the retained violation.
    persist(
        store,
        id,
        &Observation {
            identity: None,
            verified: false,
        },
        &live,
    )
    .await
    .unwrap();
    assert!(receiver.try_recv().is_err());
    let cancel = CancellationToken::new();
    let _cancel_on_exit = cancel.clone().drop_guard();
    let watcher = tokio::spawn(watch(
        client.clone(),
        store.clone(),
        id,
        IdentityLock::default(),
        live,
        cancel.clone(),
    ));
    expect_warning(&mut receiver, id, true, true).await;
    let account = store.get_account(id).await.unwrap().unwrap();
    assert!(account.own_identity_changed);
    assert!(!account.verified);

    // An identity-only query leaves the device verification subscriber
    // unchanged. The next unsigned rotation must still reach the watcher.
    rotate().await;
    expect_warning(&mut receiver, id, true, true).await;

    let current = signer
        .bootstrap_cross_signing(false)
        .await
        .unwrap()
        .upload_signing_keys_req;
    let own_device = encryption.get_own_device().await.unwrap().unwrap();
    let unsigned_device = json!(own_device.as_device_keys());
    let mut query = get_keys::v3::Response::new();
    query.device_keys = serde_json::from_value(json!({own: {device: unsigned_device}})).unwrap();
    signer
        .mark_request_as_sent(&TransactionId::new(), &query)
        .await
        .unwrap();

    // The other session signs Axon's actual device with the replacement key.
    let device_to_sign = signer.get_device(own, device, None).await.unwrap().unwrap();
    let signed_devices = || async {
        let signatures = json!(device_to_sign.verify().await.unwrap().signed_keys);
        let signatures = signatures[own.as_str()][device.as_str()]["signatures"][own.as_str()]
            .as_object()
            .unwrap();
        // /keys/signatures/upload adds signatures to the device's original keys.
        let mut device_keys = unsigned_device.clone();
        device_keys["signatures"][own.as_str()]
            .as_object_mut()
            .unwrap()
            .extend(signatures.clone());
        json!({own: {device: device_keys}})
    };
    let devices = signed_devices().await;
    *keys.lock().unwrap() = key_query(own, &current, devices);
    encryption
        .request_user_identity(own)
        .await
        .unwrap()
        .unwrap();
    expect_warning(&mut receiver, id, false, false).await;
    let account = store.get_account(id).await.unwrap().unwrap();
    assert!(!account.own_identity_changed);
    assert!(account.verified);

    let retry = retry_after_recovery(
        client,
        id,
        Observation {
            identity: None,
            verified: true,
        },
    )
    .await;
    assert!(retry.verified);
    assert!(
        retry.identity.is_some(),
        "retry pairs trust with the observed master key"
    );

    // A replacement already seen with a valid device signature still reports
    // the rotation while the durable warning remains clear.
    let next = signer
        .bootstrap_cross_signing(true)
        .await
        .unwrap()
        .upload_signing_keys_req;
    let devices = signed_devices().await;
    *keys.lock().unwrap() = key_query(own, &next, devices);
    encryption
        .request_user_identity(own)
        .await
        .unwrap()
        .unwrap();
    expect_warning(&mut receiver, id, false, true).await;

    cancel.cancel();
    watcher.await.unwrap();
}
