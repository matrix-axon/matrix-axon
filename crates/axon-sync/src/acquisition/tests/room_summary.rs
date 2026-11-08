//! Room summaries against PostgreSQL with a scripted homeserver (ADR 0128
//! step 4). The fixture's room is a joined space that links `CHILD`.
use super::hierarchy::state_in;
use super::*;
use axon_store::{RoomSummarySnapshot, SummaryStep};
use summary::SummaryEndpoint;

const SPACE: &str = ROOM;
const CHILD: &str = "!child:localhost";

fn summary_path(room: &str) -> String {
    format!("room_summary/{room}?via=localhost&via=other.org")
}

fn answer(room: &str, name: &str) -> Reply {
    Reply::Ok(
        serde_json::to_vec(&json!({
            "room_id": room,
            "name": name,
            "num_joined_members": 4,
            "join_rule": "public",
            "membership": "leave",
        }))
        .unwrap(),
    )
}

impl Fixture {
    async fn link_from(&self, space: &str, child: &str, via: Value) {
        state_in(
            &self.store,
            self.account_id,
            &self.user,
            space,
            "m.space.child",
            child,
            json!({ "via": via }),
        )
        .await;
    }

    /// Link `CHILD` from the space through two servers, and an invalid one.
    async fn link_child(&self) {
        self.link_from(
            SPACE,
            CHILD,
            json!(["localhost", "not a server", "other.org"]),
        )
        .await;
    }

    async fn membership(&self, room: &str, membership: &str) {
        let user = self.user.clone();
        state_in(
            &self.store,
            self.account_id,
            &user,
            room,
            "m.room.member",
            &user,
            json!({ "membership": membership }),
        )
        .await;
    }

    /// Queue a summary job for `room`, optionally let a read see it, and run it.
    async fn summarize(&mut self, room: &str, observed: bool) {
        self.worker
            .queue
            .push(room.try_into().unwrap(), RoomDetailJob::Summary);
        if observed {
            assert_eq!(
                self.worker.queue.observe(room, RoomDetailJob::Summary),
                JobActivity::Queued
            );
        }
        let (room_id, job) = self.worker.queue.next().await;
        self.worker.step(&room_id, job).await;
    }

    fn frames_for(&self, room: &str) -> Vec<RoomDetailSource> {
        self.changes
            .take_pending()
            .remove(room)
            .map(|sources| sources.into_iter().collect())
            .unwrap_or_default()
    }

    async fn summary(&self, room: &str) -> RoomSummarySnapshot {
        self.store
            .room_summary_snapshot(self.account_id, room)
            .await
            .unwrap()
    }

    fn versions_reads(&self) -> usize {
        self.script.versions_reads.load(Ordering::SeqCst)
    }

    /// Let the summary and any backoff or `unsupported` answer expire.
    async fn expire_summary(&self) {
        sqlx_core::query::query(
            "WITH refresh AS (UPDATE room_discovery_refresh SET next_attempt_at = 0 \
                 WHERE account_id = $1) \
             UPDATE room_discovery SET expires_at = 0 WHERE account_id = $1",
        )
        .bind(self.account_id)
        .execute(self.store.pool())
        .await
        .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_linked_unjoined_room_is_summarized_once_until_it_expires() {
    let mut fx = fixture().await;
    fx.link_child().await;
    let before = fx.summary(CHILD).await;
    assert!(before.eligible && before.due && !before.joined);
    assert!(before.discovered.is_none() && before.refresh.is_none());

    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    fx.summarize(CHILD, false).await;
    assert_eq!(
        fx.script.take_calls(),
        [summary_path(CHILD)],
        "via comes from the link: valid servers only, in order"
    );
    assert_eq!(fx.frames_for(CHILD), [RoomDetailSource::Discovery]);
    let after = fx.summary(CHILD).await;
    let row = after.discovered.unwrap();
    assert_eq!(
        (row.status.as_str(), row.source.as_str()),
        ("available", "summary")
    );
    assert_eq!(
        row.summary,
        Some(json!({"name": "Child", "num_joined_members": 4, "join_rule": "public"})),
        "the response's membership is ignored"
    );
    assert_eq!(row.expires_at - row.observed_at, ms(summary::SUMMARY_TTL));
    assert!(!after.due);
    let refresh = after.refresh.unwrap();
    assert_eq!(
        (
            refresh.support.as_str(),
            refresh.failure,
            refresh.next_attempt_at
        ),
        ("supported", None, None)
    );

    // Fresh: another job asks nothing and announces nothing.
    fx.summarize(CHILD, false).await;
    assert!(fx.script.take_calls().is_empty());
    assert!(fx.frames_for(CHILD).is_empty());
    // A panel that saw it queued still hears that it ended.
    fx.summarize(CHILD, true).await;
    assert!(fx.script.take_calls().is_empty());
    assert_eq!(fx.frames_for(CHILD), [RoomDetailSource::Discovery]);

    // Expired, an identical answer is asked again but publishes nothing.
    fx.expire_summary().await;
    assert!(fx.summary(CHILD).await.due);
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.script.take_calls(), [summary_path(CHILD)]);
    assert!(fx.frames_for(CHILD).is_empty());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn via_comes_from_every_joined_linking_space_up_to_three_servers() {
    let mut fx = fixture().await;
    // A second joined space, ordered before the fixture's, links it too.
    let other = "!aaa-space:localhost";
    sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, $2, 1, '$fixture', 1, false)")
        .bind(fx.account_id).bind(other).execute(fx.store.pool()).await.unwrap();
    fx.membership(other, "join").await;
    fx.link_from(other, CHILD, json!(["first.org", "localhost"]))
        .await;
    fx.link_from(
        SPACE,
        CHILD,
        json!(["localhost", "second.org", "third.org"]),
    )
    .await;
    // A space the account has left contributes nothing.
    let left = "!aab-space:localhost";
    sqlx_core::query::query("INSERT INTO room_summaries (account_id, room_id, last_activity_ts, last_event_id, last_event_row_id, last_activity_is_content) VALUES ($1, $2, 1, '$fixture', 1, false)")
        .bind(fx.account_id).bind(left).execute(fx.store.pool()).await.unwrap();
    fx.link_from(left, CHILD, json!(["left.org"])).await;

    fx.summarize(CHILD, false).await;
    assert_eq!(
        fx.script.take_calls(),
        [format!(
            "room_summary/{CHILD}?via=first.org&via=localhost&via=second.org"
        )]
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn the_unstable_path_and_an_unsupported_homeserver() {
    let mut fx = fixture().await;
    fx.link_child().await;
    *fx.script.summary_endpoint.lock().unwrap() = Some(SummaryEndpoint::Unstable);
    let unstable = format!("im.nheko.summary/rooms/{CHILD}/summary?via=localhost&via=other.org");
    fx.script.reply(unstable.clone(), answer(CHILD, "Child"));
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.script.take_calls(), [unstable]);
    assert_eq!(
        fx.summary(CHILD)
            .await
            .discovered
            .and_then(|row| row.summary)
            .and_then(|summary| summary.get("name").cloned()),
        Some(json!("Child"))
    );

    // A homeserver with neither gets no request, and the room's summary must
    // come from a crawl of a linking space until the answer expires.
    fx.expire_summary().await;
    *fx.script.summary_endpoint.lock().unwrap() = Some(SummaryEndpoint::Unsupported);
    fx.summarize(CHILD, false).await;
    assert!(fx.script.take_calls().is_empty());
    assert_eq!(fx.frames_for(CHILD), [RoomDetailSource::Discovery]);
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.unsupported && !snapshot.due);
    let refresh = snapshot.refresh.unwrap();
    assert_eq!(refresh.support, "unsupported");
    assert!(refresh.next_attempt_at.is_some());
    assert_eq!(
        snapshot.discovered.unwrap().status,
        "available",
        "unsupported keeps the last answer"
    );
    assert_eq!(
        fx.store
            .room_summary_fallback_space(fx.account_id, CHILD)
            .await
            .unwrap()
            .as_deref(),
        Some(SPACE)
    );
    let reads = fx.versions_reads();
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.versions_reads(), reads, "not due: /versions is not read");

    // A failed check after the 24 hours keeps `unsupported`, and with it the
    // crawl fallback, through the backoff.
    fx.expire_summary().await;
    *fx.script.summary_endpoint.lock().unwrap() = Some(SummaryEndpoint::Unknown);
    fx.summarize(CHILD, false).await;
    assert!(fx.script.take_calls().is_empty());
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.unsupported && !snapshot.due);
    let refresh = snapshot.refresh.unwrap();
    assert_eq!(
        (refresh.support.as_str(), refresh.failure.as_deref()),
        ("unsupported", Some("transport"))
    );
    assert_eq!(
        fx.store
            .room_summary_fallback_space(fx.account_id, CHILD)
            .await
            .unwrap()
            .as_deref(),
        Some(SPACE)
    );

    // A missing endpoint in the answer says the same.
    fx.expire_summary().await;
    *fx.script.summary_endpoint.lock().unwrap() = None;
    fx.script
        .reply(summary_path(CHILD), status(404, Some("M_UNRECOGNIZED")));
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.script.take_calls(), [summary_path(CHILD)]);
    assert!(fx.summary(CHILD).await.unsupported);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn denials_clear_the_fields_and_failures_back_off() {
    let mut fx = fixture().await;
    fx.link_child().await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    fx.summarize(CHILD, false).await;

    fx.expire_summary().await;
    fx.script
        .reply(summary_path(CHILD), status(404, Some("M_NOT_FOUND")));
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.frames_for(CHILD), [RoomDetailSource::Discovery]);
    let row = fx.summary(CHILD).await.discovered.unwrap();
    assert_eq!(
        (row.status.as_str(), row.source.as_str(), row.summary),
        ("not_found", "summary", None)
    );
    assert_eq!(row.expires_at - row.observed_at, ms(CONCLUSIVE_DENIAL_TTL));

    fx.expire_summary().await;
    fx.script
        .reply(summary_path(CHILD), status(403, Some("M_FORBIDDEN")));
    fx.summarize(CHILD, false).await;
    assert_eq!(
        fx.summary(CHILD).await.discovered.unwrap().status,
        "forbidden"
    );

    // A failure keeps the last answer and backs off.
    fx.expire_summary().await;
    fx.script.take_calls();
    fx.script.reply(summary_path(CHILD), status(502, None));
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.script.take_calls(), [summary_path(CHILD)]);
    assert_eq!(fx.frames_for(CHILD), [RoomDetailSource::Discovery]);
    let snapshot = fx.summary(CHILD).await;
    assert!(!snapshot.due && !snapshot.unsupported);
    assert_eq!(snapshot.discovered.unwrap().status, "forbidden");
    let refresh = snapshot.refresh.unwrap();
    assert_eq!(refresh.failure.as_deref(), Some("server_error"));
    assert!(refresh.next_attempt_at.is_some());
    fx.summarize(CHILD, false).await;
    assert!(
        fx.script.take_calls().is_empty(),
        "no request during the backoff"
    );

    // The same failure again publishes nothing; an answer clears it.
    fx.expire_summary().await;
    fx.summarize(CHILD, false).await;
    assert!(fx.frames_for(CHILD).is_empty());
    fx.expire_summary().await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Back"));
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.frames_for(CHILD), [RoomDetailSource::Discovery]);
    let refresh = fx.summary(CHILD).await.refresh.unwrap();
    assert_eq!((refresh.failure, refresh.next_attempt_at), (None, None));
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn joined_unlinked_and_unknown_rooms_get_no_request() {
    let mut fx = fixture().await;
    // The joined space itself, a room nothing links, and an unknown room.
    for room in [SPACE, "!unlinked:localhost", "!unknown:localhost"] {
        assert!(!fx.summary(room).await.eligible, "{room}");
        fx.summarize(room, false).await;
    }
    // A joined child is not summarized either.
    fx.link_child().await;
    fx.membership(CHILD, "join").await;
    let joined = fx.summary(CHILD).await;
    assert!(joined.joined && !joined.eligible && !joined.due);
    fx.summarize(CHILD, false).await;
    // A link from a space the account has left qualifies nothing.
    fx.membership(CHILD, "leave").await;
    fx.membership(SPACE, "leave").await;
    assert!(!fx.summary(CHILD).await.eligible);
    fx.summarize(CHILD, false).await;
    assert!(fx.script.take_calls().is_empty());
    assert_eq!(fx.versions_reads(), 0);
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_join_of_the_room_during_a_request_discards_its_result() {
    let mut fx = fixture().await;
    fx.link_child().await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    let (store, account_id, user) = (fx.store.clone(), fx.account_id, fx.user.clone());
    *fx.script.during_request.lock().unwrap() = Some(Box::new(move || {
        Box::pin(async move {
            state_in(
                &store,
                account_id,
                &user,
                CHILD,
                "m.room.member",
                &user,
                json!({"membership": "join"}),
            )
            .await;
        })
    }));
    fx.summarize(CHILD, false).await;
    assert_eq!(fx.script.take_calls(), [summary_path(CHILD)]);
    assert!(fx.frames_for(CHILD).is_empty());
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.discovered.is_none() && snapshot.refresh.is_none());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_leave_or_unlink_during_a_request_commits_nothing() {
    for change in ["leave_space", "unlink"] {
        let mut fx = fixture().await;
        fx.link_child().await;
        fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
        let (store, account_id, user) = (fx.store.clone(), fx.account_id, fx.user.clone());
        *fx.script.during_request.lock().unwrap() = Some(Box::new(move || {
            Box::pin(async move {
                let (room, event_type, key, content) = if change == "leave_space" {
                    (
                        SPACE,
                        "m.room.member",
                        user.clone(),
                        json!({"membership": "leave"}),
                    )
                } else {
                    (SPACE, "m.space.child", CHILD.to_owned(), json!({}))
                };
                state_in(&store, account_id, &user, room, event_type, &key, content).await;
            })
        }));
        fx.summarize(CHILD, false).await;
        assert_eq!(fx.script.take_calls(), [summary_path(CHILD)], "{change}");
        assert!(fx.frames_for(CHILD).is_empty(), "{change}");
        let snapshot = fx.summary(CHILD).await;
        assert!(
            snapshot.discovered.is_none() && snapshot.refresh.is_none(),
            "{change}"
        );
    }
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn leaving_the_last_linking_space_removes_the_summary_and_its_refresh() {
    let mut fx = fixture().await;
    fx.link_child().await;
    fx.script.reply(summary_path(CHILD), status(502, None));
    fx.summarize(CHILD, false).await;
    assert!(fx.summary(CHILD).await.refresh.is_some());
    fx.expire_summary().await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    fx.summarize(CHILD, false).await;
    assert!(fx.summary(CHILD).await.discovered.is_some());

    fx.membership(SPACE, "leave").await;
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.discovered.is_none() && snapshot.refresh.is_none());
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn joining_the_room_removes_its_summary_and_refresh() {
    let mut fx = fixture().await;
    fx.link_child().await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    fx.summarize(CHILD, false).await;
    // A first join: the child has no summary row whose epoch could advance.
    fx.membership(CHILD, "join").await;
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.discovered.is_none() && snapshot.refresh.is_none());
}

/// A summary taken while a crawl of the linking space is under way is newer
/// than the crawl's generation, so its completion keeps it even though the
/// crawl never returned the room (ADR 0128 decision 3).
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_summary_during_a_crawl_is_not_marked_not_returned() {
    let mut fx = fixture().await;
    fx.link_child().await;
    let first = format!("rooms/{SPACE}/hierarchy?max_depth=1&suggested_only=false&limit=50");
    fx.script.reply(
        first.clone(),
        Reply::Ok(serde_json::to_vec(&json!({"rooms": [], "next_batch": "t1"})).unwrap()),
    );
    fx.script.reply(
        format!("{first}&from=t1"),
        Reply::Ok(serde_json::to_vec(&json!({"rooms": []})).unwrap()),
    );
    // An earlier hierarchy summary that this crawl will not return.
    sqlx_core::query::query(
        "INSERT INTO room_discovery (account_id, room_id, status, source, summary, \
             observed_at, expires_at) VALUES ($1, $2, 'available', 'hierarchy', '{}', 0, 0)",
    )
    .bind(fx.account_id)
    .bind(CHILD)
    .execute(fx.store.pool())
    .await
    .unwrap();
    fx.run(RoomDetailJob::Hierarchy, false).await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    // Between the pages, as a panel's job is served.
    let child = <&RoomId>::try_from(CHILD).unwrap();
    fx.worker.step(child, RoomDetailJob::Summary).await;
    let (room_id, job) = fx.worker.queue.next().await;
    assert_eq!(job, RoomDetailJob::Hierarchy);
    fx.worker.step(&room_id, job).await;
    let row = fx.summary(CHILD).await.discovered.unwrap();
    assert_eq!(
        (row.status.as_str(), row.source.as_str()),
        ("available", "summary")
    );
}

/// A join's member-event transaction deletes the room's discovery row and
/// then its refresh row. A summary commit arriving while the join holds the
/// first must wait for it, not lock the refresh row and deadlock with it,
/// and then see the join.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn a_summary_commit_waits_for_a_join_that_holds_the_discovery_row() {
    let mut fx = fixture().await;
    fx.link_child().await;
    fx.script.reply(summary_path(CHILD), answer(CHILD, "Child"));
    fx.summarize(CHILD, false).await;
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.discovered.is_some() && snapshot.refresh.is_some());

    let pool = fx.store.pool().clone();
    let mut join = pool.begin().await.unwrap();
    sqlx_core::query::query(
        "INSERT INTO room_state (account_id, room_id, event_type, state_key, event_id, \
             sender, origin_ts, content) \
         VALUES ($1, $2, 'm.room.member', $3, '$join', $3, 1, '{\"membership\":\"join\"}')",
    )
    .bind(fx.account_id)
    .bind(CHILD)
    .bind(&fx.user)
    .execute(&mut *join)
    .await
    .unwrap();
    sqlx_core::query::query("DELETE FROM room_discovery WHERE account_id = $1 AND room_id = $2")
        .bind(fx.account_id)
        .bind(CHILD)
        .execute(&mut *join)
        .await
        .unwrap();

    let (store, account_id) = (fx.store.clone(), fx.account_id);
    let commit = tokio::spawn(async move {
        store
            .record_room_summary(
                account_id,
                CHILD,
                summary::limits(),
                &SummaryStep::Denied {
                    status: "not_found",
                    ttl_ms: 60_000,
                },
            )
            .await
    });
    // Until the summary commit is waiting on a row lock.
    let mut waiting = false;
    for _ in 0..100 {
        let (count,): (i64,) = sqlx_core::query_as::query_as(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND wait_event_type = 'Lock' \
               AND query LIKE '%room_discovery%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        if count > 0 {
            waiting = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(waiting, "the summary commit never waited for the join");

    sqlx_core::query::query(
        "DELETE FROM room_discovery_refresh WHERE account_id = $1 AND room_id = $2",
    )
    .bind(fx.account_id)
    .bind(CHILD)
    .execute(&mut *join)
    .await
    .expect("the join's refresh delete must not deadlock");
    join.commit().await.unwrap();

    let write = commit
        .await
        .unwrap()
        .expect("the summary commit must not fail");
    assert!(write.fenced, "the join committed first");
    let snapshot = fx.summary(CHILD).await;
    assert!(snapshot.discovered.is_none() && snapshot.refresh.is_none());
}
