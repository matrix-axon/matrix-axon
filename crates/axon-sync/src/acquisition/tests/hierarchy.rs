//! Hierarchy crawls against PostgreSQL with a scripted homeserver (ADR 0128
//! step 3). The fixture's room is the space.
use super::*;
use axon_store::{CrawlWrite, SpaceCrawlSnapshot};

const SPACE: &str = ROOM;

fn hierarchy_path(limit: i32, from: Option<&str>) -> String {
    let mut path =
        format!("rooms/{SPACE}/hierarchy?max_depth=1&suggested_only=false&limit={limit}");
    if let Some(from) = from {
        path.push_str(&format!("&from={from}"));
    }
    path
}

fn page(rooms: Value, next_batch: Option<&str>) -> Reply {
    let mut body = json!({ "rooms": rooms });
    if let Some(next) = next_batch {
        body["next_batch"] = json!(next);
    }
    Reply::Ok(serde_json::to_vec(&body).unwrap())
}

fn child(id: &str, name: &str) -> Value {
    json!({"room_id": id, "name": name, "num_joined_members": 3, "children_state": []})
}

async fn state_in(
    store: &Store,
    account_id: Uuid,
    user: &str,
    room_id: &str,
    event_type: &str,
    key: &str,
    content: Value,
) {
    let event_id = format!("$state-{}", Uuid::new_v4());
    store
        .upsert_room_state_for_local_user(
            &axon_store::RoomStateUpsert {
                account_id,
                room_id,
                event_type,
                state_key: key,
                event_id: &event_id,
                sender: user,
                origin_ts: chrono::Utc::now().timestamp_millis(),
                content: Some(content),
            },
            Some(user),
        )
        .await
        .unwrap();
}

impl Fixture {
    async fn link(&self, child: &str) {
        state_in(
            &self.store,
            self.account_id,
            &self.user,
            SPACE,
            "m.space.child",
            child,
            json!({"via": ["localhost"]}),
        )
        .await;
    }

    async fn unlink(&self, child: &str) {
        state_in(
            &self.store,
            self.account_id,
            &self.user,
            SPACE,
            "m.space.child",
            child,
            json!({}),
        )
        .await;
    }

    /// Ask for a crawl as an `enrich` read would, and run its first page.
    async fn crawl(&mut self) {
        self.run(RoomDetailJob::Hierarchy, false).await;
    }

    /// Run the next queued job, which must be the crawl's next page.
    async fn next_page(&mut self) {
        let (room_id, job) = self.worker.queue.next().await;
        assert_eq!(job, RoomDetailJob::Hierarchy);
        self.worker.step(&room_id, job).await;
    }

    fn page_queued(&self) -> bool {
        self.worker.queue.depth() > 0
    }

    async fn snapshot(&self) -> SpaceCrawlSnapshot {
        self.store
            .space_crawl_snapshot(self.account_id, SPACE)
            .await
            .unwrap()
    }

    /// Each child's discovered status and name, in hierarchy order.
    async fn discovered(&self) -> Vec<(String, Option<String>, Option<String>)> {
        let page = self
            .store
            .space_hierarchy_page(self.account_id, SPACE, None, 500)
            .await
            .unwrap();
        page.rows
            .into_iter()
            .map(|row| {
                let status = row.discovered.as_ref().map(|d| d.status.clone());
                let name = row
                    .discovered
                    .as_ref()
                    .and_then(|d| d.summary.as_ref())
                    .and_then(|s| s.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                (row.child.room_id, status, name)
            })
            .collect()
    }

    /// Let the crawl's TTL and any backoff pass.
    async fn expire(&self) {
        sqlx_core::query::query(
            "UPDATE space_discovery SET expires_at = 0, next_attempt_at = NULL \
             WHERE account_id = $1",
        )
        .bind(self.account_id)
        .execute(self.store.pool())
        .await
        .unwrap();
    }

    async fn record(&self, generation: i64, step: &axon_store::CrawlStep) -> CrawlWrite {
        let epoch = self
            .store
            .joined_epoch(self.account_id, SPACE)
            .await
            .unwrap()
            .unwrap();
        self.store
            .record_space_crawl(
                self.account_id,
                SPACE,
                epoch,
                generation,
                super::super::hierarchy::limits(),
                step,
            )
            .await
            .unwrap()
    }
}

fn row(id: &str, status: &str, name: Option<&str>) -> (String, Option<String>, Option<String>) {
    (
        id.to_owned(),
        Some(status.to_owned()),
        name.map(str::to_owned),
    )
}

fn none(id: &str) -> (String, Option<String>, Option<String>) {
    (id.to_owned(), None, None)
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn names_fill_in_page_by_page_and_the_crawl_completes_once() {
    let mut fx = fixture().await;
    for id in ["!a:localhost", "!b:localhost", "!c:localhost"] {
        fx.link(id).await;
    }
    fx.script.reply(
        hierarchy_path(50, None),
        page(
            json!([
                child(SPACE, "space"),
                child("!a:localhost", "A"),
                child("!b:localhost", "B")
            ]),
            Some("t1"),
        ),
    );
    fx.script.reply(
        hierarchy_path(50, Some("t1")),
        page(
            json!([
                child("!c:localhost", "C"),
                child("!unlinked:localhost", "X")
            ]),
            None,
        ),
    );

    fx.crawl().await;
    assert_eq!(fx.script.take_calls(), [hierarchy_path(50, None)]);
    assert_eq!(fx.frames(), [RoomDetailSource::SpaceHierarchy]);
    assert!(fx.page_queued(), "the next page queues itself");
    assert_eq!(
        fx.discovered().await,
        [
            row("!a:localhost", "available", Some("A")),
            row("!b:localhost", "available", Some("B")),
            none("!c:localhost"),
        ]
    );
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        (crawl.outcome.as_str(), crawl.pages, crawl.rooms_cached),
        ("in_progress", 1, 2)
    );

    fx.next_page().await;
    assert_eq!(fx.script.take_calls(), [hierarchy_path(50, Some("t1"))]);
    assert_eq!(fx.frames(), [RoomDetailSource::SpaceHierarchy]);
    assert!(!fx.page_queued());
    assert_eq!(
        fx.discovered().await,
        [
            row("!a:localhost", "available", Some("A")),
            row("!b:localhost", "available", Some("B")),
            row("!c:localhost", "available", Some("C")),
        ],
        "an unlinked room the homeserver returned is dropped"
    );
    let snapshot = fx.snapshot().await;
    let crawl = snapshot.crawl.unwrap();
    assert_eq!(
        (
            crawl.outcome.as_str(),
            crawl.pages,
            crawl.rooms_cached,
            crawl.support.as_str()
        ),
        ("complete", 2, 3, "supported")
    );
    assert!(crawl.completed_at.is_some());
    assert!(!snapshot.due, "a fresh complete crawl is not due");
    assert!(fx
        .store
        .room_discovery(fx.account_id, "!unlinked:localhost")
        .await
        .unwrap()
        .is_none());

    // Until it expires, another crawl job asks nothing.
    fx.crawl().await;
    assert!(fx.script.take_calls().is_empty());
    assert!(fx.frames().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn identical_pages_announce_only_the_end_of_the_crawl() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A")]), Some("t1")),
    );
    fx.script.reply(
        hierarchy_path(50, Some("t1")),
        page(json!([child("!b:localhost", "B")]), None),
    );
    fx.crawl().await;
    fx.next_page().await;
    fx.frames();
    fx.expire().await;
    assert!(fx.snapshot().await.due);

    fx.crawl().await;
    assert!(
        fx.frames().is_empty(),
        "an identical page publishes nothing"
    );
    fx.next_page().await;
    assert_eq!(
        fx.frames(),
        [RoomDetailSource::SpaceHierarchy],
        "the end of a crawl is always announced"
    );
    assert_eq!(fx.snapshot().await.crawl.unwrap().generation, 2);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn max_pages_truncates_and_a_repeated_token_is_a_loop() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.script
        .reply(hierarchy_path(50, None), page(json!([]), Some("t1")));
    for n in 1..=super::super::hierarchy::MAX_PAGES {
        fx.script.reply(
            hierarchy_path(50, Some(&format!("t{n}"))),
            page(json!([]), Some(&format!("t{}", n + 1))),
        );
    }
    fx.crawl().await;
    let mut pages = 1;
    while fx.page_queued() {
        fx.next_page().await;
        pages += 1;
    }
    assert_eq!(pages, super::super::hierarchy::MAX_PAGES);
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        (crawl.outcome.as_str(), crawl.failure.as_deref()),
        ("truncated", None)
    );
    assert!(
        !fx.snapshot().await.due,
        "truncated is conclusive until it expires"
    );

    fx.expire().await;
    fx.script.take_calls();
    fx.script
        .reply(hierarchy_path(50, Some("t1")), page(json!([]), Some("t1")));
    fx.crawl().await;
    fx.next_page().await;
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        (crawl.outcome.as_str(), crawl.failure.as_deref()),
        ("truncated", Some("pagination_loop"))
    );
    assert_eq!(fx.script.take_calls().len(), 2);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_expired_cursor_restarts_once_then_fails_with_backoff() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A")]), Some("t1")),
    );
    fx.script.reply(
        hierarchy_path(50, Some("t1")),
        status(400, Some("M_INVALID_PARAM")),
    );
    fx.crawl().await;
    let generation = fx.snapshot().await.crawl.unwrap().generation;
    fx.next_page().await;
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        crawl.generation,
        generation + 1,
        "a restart is a new generation"
    );
    assert_eq!((crawl.outcome.as_str(), crawl.pages), ("in_progress", 0));
    assert!(
        fx.page_queued(),
        "the restart asks for the first page again"
    );

    fx.next_page().await;
    fx.next_page().await;
    let snapshot = fx.snapshot().await;
    let crawl = snapshot.crawl.unwrap();
    assert_eq!(
        (crawl.outcome.as_str(), crawl.failure.as_deref()),
        ("failed", Some("cursor_expired"))
    );
    assert!(crawl.next_attempt_at.is_some());
    assert!(!snapshot.due, "backing off");
    assert!(!fx.page_queued());
    assert_eq!(
        fx.script.take_calls(),
        [
            hierarchy_path(50, None),
            hierarchy_path(50, Some("t1")),
            hierarchy_path(50, None),
            hierarchy_path(50, Some("t1")),
        ]
    );

    // After the backoff, the crawl starts over from the first page.
    fx.expire().await;
    fx.crawl().await;
    assert_eq!(fx.script.take_calls(), [hierarchy_path(50, None)]);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_oversized_page_halves_the_limit_for_the_rest_of_the_crawl() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(hierarchy_path(50, None), Reply::TooLarge);
    fx.script.reply(
        hierarchy_path(25, None),
        page(json!([child("!a:localhost", "A")]), Some("t1")),
    );
    fx.script
        .reply(hierarchy_path(25, Some("t1")), Reply::TooLarge);
    fx.script.reply(
        hierarchy_path(12, Some("t1")),
        page(json!([child("!b:localhost", "B")]), None),
    );
    fx.crawl().await;
    while fx.page_queued() {
        fx.next_page().await;
    }
    assert_eq!(
        fx.script.take_calls(),
        [
            hierarchy_path(50, None),
            hierarchy_path(25, None),
            hierarchy_path(25, Some("t1")),
            hierarchy_path(12, Some("t1")),
        ],
        "a later page keeps the lower limit and is never raised"
    );
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!((crawl.outcome.as_str(), crawl.page_limit), ("complete", 12));

    // A single room over the limit ends the crawl.
    fx.expire().await;
    fx.script.replies.lock().unwrap().clear();
    for limit in [50, 25, 12, 6, 3, 1] {
        fx.script
            .reply(hierarchy_path(limit, None), Reply::TooLarge);
    }
    fx.crawl().await;
    while fx.page_queued() {
        fx.next_page().await;
    }
    let calls = fx.script.take_calls();
    assert_eq!(calls.len(), 6, "50, 25, 12, 6, 3, 1: {calls:?}");
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        (crawl.outcome.as_str(), crawl.failure.as_deref()),
        ("truncated", Some("too_large"))
    );
    assert_eq!(
        fx.discovered().await[0],
        row("!a:localhost", "available", Some("A")),
        "a truncated crawl cannot prove absence"
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_failure_keeps_committed_pages_and_resumes_from_the_cursor() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A")]), Some("t1")),
    );
    fx.script
        .reply(hierarchy_path(50, Some("t1")), status(502, None));
    fx.crawl().await;
    fx.frames();
    fx.next_page().await;
    assert_eq!(fx.frames(), [RoomDetailSource::SpaceHierarchy]);
    let snapshot = fx.snapshot().await;
    let crawl = snapshot.crawl.clone().unwrap();
    assert_eq!(
        (
            crawl.outcome.as_str(),
            crawl.failure.as_deref(),
            crawl.pages
        ),
        ("failed", Some("server_error"), 1)
    );
    assert!(!snapshot.due);
    assert_eq!(
        fx.discovered().await[0],
        row("!a:localhost", "available", Some("A"))
    );

    // Out of backoff, the crawl resumes in the same generation.
    sqlx_core::query::query("UPDATE space_discovery SET next_attempt_at = 0 WHERE account_id = $1")
        .bind(fx.account_id)
        .execute(fx.store.pool())
        .await
        .unwrap();
    assert!(fx.snapshot().await.due);
    fx.script.take_calls();
    fx.script.reply(
        hierarchy_path(50, Some("t1")),
        page(json!([child("!b:localhost", "B")]), None),
    );
    fx.crawl().await;
    assert_eq!(fx.script.take_calls(), [hierarchy_path(50, Some("t1"))]);
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        (
            crawl.outcome.as_str(),
            crawl.generation,
            crawl.pages,
            crawl.failure
        ),
        ("complete", 1, 2, None)
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn unsupported_and_denied_spaces_wait_out_their_expiration() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        status(404, Some("M_UNRECOGNIZED")),
    );
    fx.crawl().await;
    assert_eq!(fx.frames(), [RoomDetailSource::SpaceHierarchy]);
    let snapshot = fx.snapshot().await;
    let crawl = snapshot.crawl.unwrap();
    assert_eq!(
        (
            crawl.outcome.as_str(),
            crawl.support.as_str(),
            crawl.failure
        ),
        ("failed", "unsupported", None)
    );
    assert!(!snapshot.due);
    let now = chrono::Utc::now().timestamp_millis();
    assert!(crawl.next_attempt_at.unwrap() > now + 23 * 3600 * 1000);

    fx.expire().await;
    fx.script
        .reply(hierarchy_path(50, None), status(403, Some("M_FORBIDDEN")));
    fx.crawl().await;
    let crawl = fx.snapshot().await.crawl.unwrap();
    assert_eq!(
        (crawl.outcome.as_str(), crawl.failure.as_deref()),
        ("failed", Some("forbidden"))
    );
    assert!(!fx.snapshot().await.due);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn only_a_complete_crawl_marks_omitted_children_not_returned() {
    let mut fx = fixture().await;
    for id in ["!a:localhost", "!b:localhost", "!c:localhost"] {
        fx.link(id).await;
    }
    fx.script.reply(
        hierarchy_path(50, None),
        page(
            json!([
                child("!a:localhost", "A"),
                child("!b:localhost", "B"),
                child("!c:localhost", "C")
            ]),
            None,
        ),
    );
    fx.crawl().await;

    // The next crawl returns a only on its first page, then fails; b and c
    // are untouched because a failed crawl proves nothing.
    fx.expire().await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A2")]), Some("t1")),
    );
    fx.script
        .reply(hierarchy_path(50, Some("t1")), status(502, None));
    fx.crawl().await;
    fx.next_page().await;
    assert_eq!(
        fx.discovered().await,
        [
            row("!a:localhost", "available", Some("A2")),
            row("!b:localhost", "available", Some("B")),
            row("!c:localhost", "available", Some("C")),
        ]
    );

    // Resumed in the same generation, it returns b with a withheld field and
    // finishes: a counts from the first page, b counts though malformed, and
    // only c is omitted.
    sqlx_core::query::query("UPDATE space_discovery SET next_attempt_at = 0 WHERE account_id = $1")
        .bind(fx.account_id)
        .execute(fx.store.pool())
        .await
        .unwrap();
    fx.frames();
    // Even if the clock stepped back since a was returned, the returned set
    // keeps a: it was returned in this generation.
    sqlx_core::query::query(
        "UPDATE room_discovery SET observed_at = 0 WHERE account_id = $1 AND room_id = '!a:localhost'",
    )
    .bind(fx.account_id)
    .execute(fx.store.pool())
    .await
    .unwrap();
    fx.script.reply(
        hierarchy_path(50, Some("t1")),
        page(
            json!([{"room_id": "!b:localhost", "name": 7, "topic": "kept"}]),
            None,
        ),
    );
    fx.crawl().await;
    assert_eq!(fx.frames(), [RoomDetailSource::SpaceHierarchy]);
    assert_eq!(
        fx.discovered().await,
        [
            row("!a:localhost", "available", Some("A2")),
            row("!b:localhost", "available", None),
            row("!c:localhost", "not_returned", None),
        ],
        "a field a newer answer drops is removed, and an omitted child keeps a status"
    );
    let b = fx
        .store
        .room_discovery(fx.account_id, "!b:localhost")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b.withheld, ["name"]);
    assert_eq!(b.summary, Some(json!({"topic": "kept"})));
    let c = fx
        .store
        .room_discovery(fx.account_id, "!c:localhost")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((c.summary, c.withheld.len()), (None, 0));
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_unidentified_entry_or_a_newer_summary_prevents_omission() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(
            json!([child("!a:localhost", "A"), child("!b:localhost", "B")]),
            None,
        ),
    );
    fx.crawl().await;

    // An entry without a room id might have been b.
    fx.expire().await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A"), {"name": "no id"}]), None),
    );
    fx.crawl().await;
    assert_eq!(
        fx.discovered().await[1],
        row("!b:localhost", "available", Some("B"))
    );

    // A summary another crawl refreshed after this generation started is kept.
    fx.expire().await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A")]), Some("t1")),
    );
    fx.script
        .reply(hierarchy_path(50, Some("t1")), page(json!([]), None));
    fx.crawl().await;
    sqlx_core::query::query(
        "UPDATE room_discovery SET observed_at = (SELECT started_at FROM space_discovery \
             WHERE account_id = $1) + 1 \
         WHERE account_id = $1 AND room_id = '!b:localhost'",
    )
    .bind(fx.account_id)
    .execute(fx.store.pool())
    .await
    .unwrap();
    fx.next_page().await;
    assert_eq!(
        fx.discovered().await[1],
        row("!b:localhost", "available", Some("B"))
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn completion_deletes_rows_no_joined_space_links() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(
            json!([child("!a:localhost", "A"), child("!b:localhost", "B")]),
            None,
        ),
    );
    fx.crawl().await;
    fx.unlink("!b:localhost").await;
    assert_eq!(
        fx.discovered().await,
        [row("!a:localhost", "available", Some("A"))]
    );

    fx.expire().await;
    fx.crawl().await;
    assert!(
        fx.store
            .room_discovery(fx.account_id, "!b:localhost")
            .await
            .unwrap()
            .is_none(),
        "the orphan step removes it"
    );
    let (children,): (i64,) = sqlx_core::query_as::query_as(
        "SELECT count(*) FROM space_discovery_children WHERE account_id = $1",
    )
    .bind(fx.account_id)
    .fetch_one(fx.store.pool())
    .await
    .unwrap();
    assert_eq!(children, 1, "the returned set drops the removed link");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_joined_child_is_skipped_and_joining_deletes_its_row() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    // The account joined b before the page commits.
    state_in(
        &fx.store,
        fx.account_id,
        &fx.user,
        "!b:localhost",
        "m.room.member",
        &fx.user.clone(),
        json!({"membership": "join"}),
    )
    .await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(
            json!([child("!a:localhost", "A"), child("!b:localhost", "B")]),
            None,
        ),
    );
    fx.crawl().await;
    assert!(fx
        .store
        .room_discovery(fx.account_id, "!b:localhost")
        .await
        .unwrap()
        .is_none());

    // Joining a discovered room deletes its row with the member event.
    state_in(
        &fx.store,
        fx.account_id,
        &fx.user,
        "!a:localhost",
        "m.room.member",
        &fx.user.clone(),
        json!({"membership": "join"}),
    )
    .await;
    assert!(fx
        .store
        .room_discovery(fx.account_id, "!a:localhost")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        fx.discovered().await,
        [none("!a:localhost"), none("!b:localhost")]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn leaving_the_space_during_a_page_commits_nothing_and_clears_discovery() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.link("!b:localhost").await;
    fx.script.reply(
        hierarchy_path(50, None),
        page(json!([child("!a:localhost", "A")]), Some("t1")),
    );
    fx.crawl().await;
    assert_eq!(
        fx.discovered().await[0],
        row("!a:localhost", "available", Some("A"))
    );

    fx.script.reply(
        hierarchy_path(50, Some("t1")),
        page(json!([child("!b:localhost", "B")]), None),
    );
    let (store, account_id, user) = (fx.store.clone(), fx.account_id, fx.user.clone());
    *fx.script.during_request.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            state_in(
                &store,
                account_id,
                &user,
                SPACE,
                "m.room.member",
                &user,
                json!({"membership": "leave"}),
            )
            .await;
        })
    }));
    fx.next_page().await;
    assert!(!fx.page_queued());
    for id in ["!a:localhost", "!b:localhost"] {
        assert!(
            fx.store
                .room_discovery(fx.account_id, id)
                .await
                .unwrap()
                .is_none(),
            "{id}: nothing survives the leave"
        );
    }
    let snapshot = fx.snapshot().await;
    assert!(
        snapshot.crawl.is_none(),
        "the crawl is deleted with the leave"
    );
    assert!(!snapshot.eligible && !snapshot.due);

    // An unjoined space gets no request.
    fx.script.take_calls();
    fx.crawl().await;
    assert!(fx.script.take_calls().is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_stale_generation_is_fenced() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.script
        .reply(hierarchy_path(50, None), page(json!([]), Some("t1")));
    fx.crawl().await;
    let generation = fx.snapshot().await.crawl.unwrap().generation;
    let write = fx
        .record(
            generation + 1,
            &axon_store::CrawlStep::Page {
                entries: Vec::new(),
                unidentified: 0,
                next_batch: None,
            },
        )
        .await;
    assert!(write.fenced);
    assert_eq!(fx.snapshot().await.crawl.unwrap().outcome, "in_progress");
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_panel_job_queued_during_a_crawl_runs_before_its_next_page() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.script
        .reply(hierarchy_path(50, None), page(json!([]), Some("t1")));
    let queue = Arc::clone(&fx.worker.queue);
    *fx.script.during_request.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            queue.push(ROOM.try_into().unwrap(), RoomDetailJob::LocalAliases);
        })
    }));
    fx.crawl().await;
    let (_, first) = fx.worker.queue.next().await;
    assert_eq!(first, RoomDetailJob::LocalAliases);
    fx.worker.queue.finish(false);
    let (_, second) = fx.worker.queue.next().await;
    assert_eq!(second, RoomDetailJob::Hierarchy);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn an_observed_crawl_announces_a_page_dropped_from_the_queue() {
    let mut fx = fixture().await;
    fx.link("!a:localhost").await;
    fx.script
        .reply(hierarchy_path(50, None), page(json!([]), Some("t1")));
    fx.run(RoomDetailJob::Hierarchy, true).await;
    assert!(fx.page_queued());
    fx.frames();
    // A burst of other panels evicts the waiting page.
    for n in 0..QUEUE_CAPACITY {
        fx.worker.queue.push(
            format!("!panel{n}:localhost").as_str().try_into().unwrap(),
            RoomDetailJob::LocalAliases,
        );
    }
    assert_eq!(fx.frames(), [RoomDetailSource::SpaceHierarchy]);
    // The next enrich read resumes the crawl from its cursor.
    let snapshot = fx.snapshot().await;
    assert_eq!(snapshot.crawl.unwrap().outcome, "in_progress");
    assert!(snapshot.due);
}
