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

// ---------------------------------------------------------------------------
// `GET …/space/hierarchy` discovery (ADR 0128 step 3)
// ---------------------------------------------------------------------------

impl Joined {
    async fn state(&self, room_id: &str, event_type: &str, state_key: &str, content: Value) {
        let user: (String,) =
            sqlx_core::query_as::query_as("SELECT user_id FROM accounts WHERE account_id = $1")
                .bind(self.account_id)
                .fetch_one(self.store.pool())
                .await
                .unwrap();
        self.store
            .upsert_room_state_for_local_user(
                &RoomStateUpsert {
                    account_id: self.account_id,
                    room_id,
                    event_type,
                    state_key,
                    event_id: &format!("${}", Uuid::new_v4()),
                    sender: &user.0,
                    origin_ts: 1,
                    content: Some(content),
                },
                Some(&user.0),
            )
            .await
            .unwrap();
    }

    fn hierarchy(&self, space_id: &str, enrich: bool) -> String {
        format!(
            "/v1/accounts/{}/rooms/{space_id}/space/hierarchy?enrich={enrich}",
            self.account_id
        )
    }

    async fn sql(&self, sql: &str) {
        sqlx_core::query::query(sql)
            .bind(self.account_id)
            .bind(&self.room_id)
            .execute(self.store.pool())
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn only_an_enrich_read_of_a_joined_space_schedules_a_crawl() {
    let f = joined_room().await;
    f.state(
        &f.room_id,
        "m.space.child",
        "!child:localhost",
        json!({"via": ["localhost"]}),
    )
    .await;

    let (status, body) = get(&f.app, &f.hierarchy(&f.room_id, false)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.acquirer.take().is_empty(), "a plain read is pure");
    assert_eq!(
        body["data"]["discovery"],
        json!({
            "state": "idle", "outcome": null, "pages": 0, "page_limit": null,
            "rooms_cached": 0, "started_at": null, "completed_at": null,
            "expires_at": null, "support": "unknown", "failure": null,
            "next_attempt_at": null,
        })
    );
    assert_eq!(body["data"]["children"][0]["discovered"], Value::Null);

    let (_, body) = get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert_eq!(body["data"]["discovery"]["state"], "queued");
    // Polling while the crawl is queued asks the worker nothing more.
    let (_, body) = get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert_eq!(body["data"]["discovery"]["state"], "queued");
    assert_eq!(f.acquirer.take(), [RoomDetailJob::Hierarchy]);

    // A space the account has not joined is never crawled.
    let (status, body) = get(&f.app, &f.hierarchy("!elsewhere:localhost", true)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.acquirer.take().is_empty());
    assert_eq!(body["data"]["discovery"]["state"], "idle");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_fresh_or_backed_off_crawl_schedules_nothing() {
    let f = joined_room().await;
    f.state(
        &f.room_id,
        "m.space.child",
        "!child:localhost",
        json!({"via": ["localhost"]}),
    )
    .await;
    let now = chrono::Utc::now().timestamp_millis();
    f.sql(&format!(
        "INSERT INTO space_discovery (account_id, space_id, generation, outcome, pages, \
             page_limit, rooms_cached, started_at, completed_at, expires_at, support) \
         VALUES ($1, $2, 3, 'complete', 2, 25, 70, {now}, {now}, {}, 'supported')",
        now + 3_600_000
    ))
    .await;
    let (_, body) = get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert!(f.acquirer.take().is_empty(), "fresh until expires_at");
    assert_eq!(
        body["data"]["discovery"],
        json!({
            "state": "idle", "outcome": "complete", "pages": 2, "page_limit": 25,
            "rooms_cached": 70, "started_at": now, "completed_at": now,
            "expires_at": now + 3_600_000, "support": "supported", "failure": null,
            "next_attempt_at": null,
        })
    );

    f.sql("UPDATE space_discovery SET expires_at = 1 WHERE account_id = $1 AND space_id = $2")
        .await;
    get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert_eq!(f.acquirer.take(), [RoomDetailJob::Hierarchy], "expired");

    f.sql(&format!(
        "UPDATE space_discovery SET outcome = 'failed', completed_at = NULL, cursor = 't9', \
             failure = 'rate_limited', failures = 2, next_attempt_at = {} \
         WHERE account_id = $1 AND space_id = $2",
        now + 60_000
    ))
    .await;
    let (_, body) = get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert!(f.acquirer.take().is_empty(), "backing off");
    assert_eq!(body["data"]["discovery"]["failure"], "rate_limited");
    assert_eq!(body["data"]["discovery"]["next_attempt_at"], now + 60_000);

    // An unfinished crawl out of backoff resumes on the next enrich read.
    f.sql("UPDATE space_discovery SET next_attempt_at = 1 WHERE account_id = $1 AND space_id = $2")
        .await;
    get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert_eq!(f.acquirer.take(), [RoomDetailJob::Hierarchy]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unjoined_children_carry_discovered_summaries_and_joined_ones_never_do() {
    let f = joined_room().await;
    for child in [
        "!a:localhost",
        "!b:localhost",
        "!c:localhost",
        "!d:localhost",
    ] {
        f.state(
            &f.room_id,
            "m.space.child",
            child,
            json!({"via": ["localhost"]}),
        )
        .await;
    }
    f.sql(
        r#"INSERT INTO room_discovery (account_id, room_id, status, source, summary, withheld,
               observed_at, expires_at) VALUES
           ($1, '!a:localhost', 'available', 'hierarchy',
            '{"name": "A", "num_joined_members": 4, "world_readable": false}', '{topic}', 10, 20),
           ($1, '!b:localhost', 'not_returned', 'hierarchy', NULL, '{}', 11, 21),
           ($1, '!c:localhost', 'available', 'hierarchy', '{"name": "C"}', '{}', 12, 22)"#,
    )
    .await;
    // The account joins c after a crawl cached it: synced state wins, and a
    // raced row would still be withheld.
    f.state(
        "!c:localhost",
        "m.room.member",
        &{
            let user: (String,) =
                sqlx_core::query_as::query_as("SELECT user_id FROM accounts WHERE account_id = $1")
                    .bind(f.account_id)
                    .fetch_one(f.store.pool())
                    .await
                    .unwrap();
            user.0
        },
        json!({"membership": "join"}),
    )
    .await;
    f.sql(
        r#"INSERT INTO room_discovery (account_id, room_id, status, source, summary,
               observed_at, expires_at)
           VALUES ($1, '!c:localhost', 'available', 'hierarchy', '{"name": "raced"}', 13, 23)"#,
    )
    .await;

    let (status, body) = get(&f.app, &f.hierarchy(&f.room_id, false)).await;
    assert_eq!(status, StatusCode::OK);
    let children = body["data"]["children"].as_array().unwrap();
    let by_id = |id: &str| {
        children
            .iter()
            .find(|c| c["room_id"] == id)
            .unwrap_or_else(|| panic!("{id}"))
            .clone()
    };
    assert_eq!(
        by_id("!a:localhost")["discovered"],
        json!({
            "status": "available", "source": "hierarchy", "observed_at": 10, "expires_at": 20,
            "withheld": ["topic"], "name": "A", "topic": null, "avatar_url": null,
            "canonical_alias": null, "num_joined_members": 4, "join_rule": null,
            "room_type": null, "world_readable": false, "guest_can_join": null,
            "encryption": null, "room_version": null,
        }),
        "a missing encryption field reads as unknown"
    );
    assert_eq!(by_id("!a:localhost")["name"], Value::Null, "never merged");
    let b = by_id("!b:localhost");
    assert_eq!(b["discovered"]["status"], "not_returned");
    assert_eq!(b["discovered"]["name"], Value::Null);
    assert_eq!(by_id("!c:localhost")["joined"], true);
    assert_eq!(by_id("!c:localhost")["discovered"], Value::Null);
    assert_eq!(by_id("!d:localhost")["discovered"], Value::Null);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_space_whose_children_are_all_joined_is_not_crawled() {
    let f = joined_room().await;
    let user: (String,) =
        sqlx_core::query_as::query_as("SELECT user_id FROM accounts WHERE account_id = $1")
            .bind(f.account_id)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    // No children at all: nothing to discover.
    get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert!(f.acquirer.take().is_empty());

    // Every linked child joined, as in a bridge's portal space.
    for child in ["!portal-a:localhost", "!portal-b:localhost"] {
        f.state(
            &f.room_id,
            "m.space.child",
            child,
            json!({"via": ["localhost"]}),
        )
        .await;
        f.state(
            child,
            "m.room.member",
            &user.0,
            json!({"membership": "join"}),
        )
        .await;
    }
    // A removed link (empty content) does not count either.
    f.state(&f.room_id, "m.space.child", "!removed:localhost", json!({}))
        .await;
    let (status, _) = get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.acquirer.take().is_empty(), "nothing to discover");

    // One unjoined child is enough.
    f.state(
        &f.room_id,
        "m.space.child",
        "!new:localhost",
        json!({"via": ["localhost"]}),
    )
    .await;
    get(&f.app, &f.hierarchy(&f.room_id, true)).await;
    assert_eq!(f.acquirer.take(), [RoomDetailJob::Hierarchy]);
}
