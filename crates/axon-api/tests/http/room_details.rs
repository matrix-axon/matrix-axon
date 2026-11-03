//! `GET …/metadata` absence fields and `GET …/homeserver_details` (ADR 0126):
//! reads return the cache at once and schedule only due, eligible jobs.
use super::*;
use axon_api::RoomDetailAcquirer;
use axon_core::{JobActivity, RoomDetailJob};
use axon_store::{DetailKind, DetailOutcome};

/// Records requests; a requested job reads as queued.
#[derive(Default)]
struct RecordingAcquirer(Mutex<Vec<(Uuid, String, RoomDetailJob)>>);

impl RecordingAcquirer {
    fn take(&self) -> Vec<RoomDetailJob> {
        std::mem::take(&mut *self.0.lock().unwrap())
            .into_iter()
            .map(|(_, _, job)| job)
            .collect()
    }
}

impl RoomDetailAcquirer for RecordingAcquirer {
    fn request(&self, account_id: Uuid, room_id: &str, job: RoomDetailJob) {
        self.0
            .lock()
            .unwrap()
            .push((account_id, room_id.to_owned(), job));
    }

    fn activity(&self, account_id: Uuid, room_id: &str, job: RoomDetailJob) -> JobActivity {
        let requested = self
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(a, r, j)| *a == account_id && r == room_id && *j == job);
        if requested {
            JobActivity::Queued
        } else {
            JobActivity::Idle
        }
    }
}

struct Joined {
    store: Store,
    account_id: Uuid,
    room_id: String,
    acquirer: Arc<RecordingAcquirer>,
    app: axum::Router,
}

async fn joined_room() -> Joined {
    let store = store().await;
    let user = format!("@details-{}:localhost", Uuid::new_v4());
    let account_id = store
        .upsert_account(&user, "https://hs.example.org")
        .await
        .unwrap()
        .account_id;
    let room_id = format!("!details-{}:localhost", Uuid::new_v4());
    sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, $2, 1, '$fixture', 1, false)")
        .bind(account_id).bind(&room_id).execute(store.pool()).await.unwrap();
    for (event_type, state_key, content) in [
        (
            "m.room.member",
            user.as_str(),
            json!({"membership": "join"}),
        ),
        ("m.room.create", "", json!({"room_version": "12"})),
    ] {
        store
            .upsert_room_state_for_local_user(
                &RoomStateUpsert {
                    account_id,
                    room_id: &room_id,
                    event_type,
                    state_key,
                    event_id: &format!("${}", Uuid::new_v4()),
                    sender: &user,
                    origin_ts: 1,
                    content: Some(content),
                },
                Some(&user),
            )
            .await
            .unwrap();
    }
    let acquirer = Arc::new(RecordingAcquirer::default());
    let app = axon_api::router(
        read_state(store.clone())
            .with_room_detail_acquisition(acquirer.clone() as Arc<dyn RoomDetailAcquirer>),
    );
    Joined {
        store,
        account_id,
        room_id,
        acquirer,
        app,
    }
}

const SNAPSHOTS: [(&str, &str); 8] = [
    ("aliases", "m.room.canonical_alias"),
    ("creation", "m.room.create"),
    ("join_rules", "m.room.join_rules"),
    ("encryption", "m.room.encryption"),
    ("power_levels", "m.room.power_levels"),
    ("server_acl", "m.room.server_acl"),
    ("history_visibility", "m.room.history_visibility"),
    ("guest_access", "m.room.guest_access"),
];

#[tokio::test]
#[ignore = "requires Postgres"]
async fn metadata_reads_schedule_absence_confirmation_and_report_it() {
    let f = joined_room().await;
    let path = format!("/v1/accounts/{}/rooms/{}/metadata", f.account_id, f.room_id);
    let (status, body) = get(&f.app, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.acquirer.take(),
        [RoomDetailJob::Absence],
        "one job per read"
    );
    let data = &body["data"];
    for (key, _) in SNAPSHOTS {
        if key == "creation" {
            assert_eq!(data[key]["status"], "available");
            assert_eq!(data[key]["absence_confirmed_at"], Value::Null);
            assert_eq!(
                data[key]["absence_check"],
                Value::Null,
                "a tuple is never confirmed"
            );
        } else {
            assert_eq!(data[key]["status"], "unknown");
            assert_eq!(data[key]["absence_confirmed_at"], Value::Null);
            assert_eq!(
                data[key]["absence_check"],
                json!({"state": "queued", "failure": null, "next_attempt_at": null})
            );
        }
    }

    // Confirm every missing type; one fails and backs off.
    let epoch = f
        .store
        .joined_epoch(f.account_id, &f.room_id)
        .await
        .unwrap()
        .unwrap();
    for (_, state_type) in SNAPSHOTS.iter().filter(|(key, _)| *key != "creation") {
        let outcome = if *state_type == "m.room.server_acl" {
            DetailOutcome::Failure {
                category: "timeout",
                retry_after_ms: None,
                min_backoff_ms: 60_000,
                max_backoff_ms: 1_800_000,
            }
        } else {
            DetailOutcome::Answer {
                status: "absent",
                value: None,
                ttl_ms: 86_400_000,
            }
        };
        f.store
            .record_room_detail(
                f.account_id,
                &f.room_id,
                epoch,
                DetailKind::Absence,
                state_type,
                &outcome,
            )
            .await
            .unwrap();
    }
    let (_, body) = get(&f.app, &path).await;
    assert!(
        f.acquirer.take().is_empty(),
        "nothing is due, so nothing is asked"
    );
    let data = &body["data"];
    assert!(data["encryption"]["absence_confirmed_at"].as_i64().unwrap() > 0);
    assert_eq!(
        data["encryption"]["status"], "unknown",
        "the status enum is unchanged"
    );
    assert_eq!(
        data["encryption"]["absence_check"],
        json!({"state": "idle", "failure": null, "next_attempt_at": null})
    );
    assert_eq!(data["server_acl"]["absence_confirmed_at"], Value::Null);
    assert_eq!(data["server_acl"]["absence_check"]["state"], "idle");
    assert_eq!(data["server_acl"]["absence_check"]["failure"], "timeout");
    assert!(data["server_acl"]["absence_check"]["next_attempt_at"]
        .as_i64()
        .is_some());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn reads_of_unjoined_and_unknown_rooms_schedule_nothing() {
    let f = joined_room().await;
    let room = format!("!unknown-{}:localhost", Uuid::new_v4());
    {
        for read in ["metadata", "homeserver_details"] {
            let (status, body) = get(
                &f.app,
                &format!("/v1/accounts/{}/rooms/{room}/{read}", f.account_id),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            if read == "metadata" {
                assert_eq!(body["data"]["encryption"]["absence_check"], Value::Null);
            } else {
                assert_eq!(body["data"]["local_aliases"]["status"], "unknown");
                assert_eq!(body["data"]["local_aliases"]["refresh"]["state"], "idle");
            }
        }
    }
    // A room the account left is not acquired either.
    let user: (String,) =
        sqlx_core::query_as::query_as("SELECT user_id FROM accounts WHERE account_id = $1")
            .bind(f.account_id)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    f.store
        .upsert_room_state_for_local_user(
            &RoomStateUpsert {
                account_id: f.account_id,
                room_id: &f.room_id,
                event_type: "m.room.member",
                state_key: &user.0,
                event_id: "$leave",
                sender: &user.0,
                origin_ts: 2,
                content: Some(json!({"membership": "leave"})),
            },
            Some(&user.0),
        )
        .await
        .unwrap();
    for read in ["metadata", "homeserver_details"] {
        get(
            &f.app,
            &format!("/v1/accounts/{}/rooms/{}/{read}", f.account_id, f.room_id),
        )
        .await;
    }
    assert!(f.acquirer.take().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn homeserver_details_read_from_cache_and_refresh_when_due() {
    let f = joined_room().await;
    let path = format!(
        "/v1/accounts/{}/rooms/{}/homeserver_details",
        f.account_id, f.room_id
    );
    let (status, body) = get(&f.app, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        f.acquirer.take(),
        [
            RoomDetailJob::LocalAliases,
            RoomDetailJob::DirectoryVisibility
        ]
    );
    let unknown = json!({
        "status": "unknown", "fetched_at": null, "expires_at": null,
        "refresh": {"state": "queued", "failure": null, "next_attempt_at": null}
    });
    let mut aliases = unknown.clone();
    aliases["aliases"] = Value::Null;
    let mut visibility = unknown;
    visibility["visibility"] = Value::Null;
    assert_eq!(
        body["data"],
        json!({"local_aliases": aliases, "directory_visibility": visibility})
    );

    let epoch = f
        .store
        .joined_epoch(f.account_id, &f.room_id)
        .await
        .unwrap()
        .unwrap();
    for (kind, status, value) in [
        (
            DetailKind::LocalAliases,
            "partial",
            Some(json!(["#a:localhost"])),
        ),
        (
            DetailKind::DirectoryVisibility,
            "available",
            Some(json!({"visibility": "private"})),
        ),
    ] {
        f.store
            .record_room_detail(
                f.account_id,
                &f.room_id,
                epoch,
                kind,
                "",
                &DetailOutcome::Answer {
                    status,
                    value,
                    ttl_ms: 900_000,
                },
            )
            .await
            .unwrap();
    }
    let (_, body) = get(&f.app, &path).await;
    assert!(f.acquirer.take().is_empty(), "fresh details are pure reads");
    let data = &body["data"];
    assert_eq!(data["local_aliases"]["status"], "partial");
    assert_eq!(data["local_aliases"]["aliases"], json!(["#a:localhost"]));
    assert_eq!(data["directory_visibility"]["visibility"], "private");
    let fetched = data["directory_visibility"]["fetched_at"].as_i64().unwrap();
    assert_eq!(
        data["directory_visibility"]["expires_at"].as_i64().unwrap(),
        fetched + 900_000
    );
    assert_eq!(data["directory_visibility"]["refresh"]["state"], "idle");

    // Stale values stay visible while their refresh is queued.
    sqlx_core::query::query(
        "UPDATE room_detail_acquisitions SET expires_at = 0 WHERE account_id = $1",
    )
    .bind(f.account_id)
    .execute(f.store.pool())
    .await
    .unwrap();
    let (_, body) = get(&f.app, &path).await;
    assert_eq!(f.acquirer.take().len(), 2);
    assert_eq!(
        body["data"]["local_aliases"]["aliases"],
        json!(["#a:localhost"])
    );
    assert_eq!(body["data"]["local_aliases"]["refresh"]["state"], "queued");
}
