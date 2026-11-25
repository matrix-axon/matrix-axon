use super::*;
use crate::api::HIERARCHY_PAGE_BYTES;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Reply {
    status: &'static str,
    body: String,
    delay: Duration,
}

impl Reply {
    fn page(value: Value) -> Self {
        Self {
            status: "200 OK",
            body: json!({"data": value}).to_string(),
            delay: Duration::ZERO,
        }
    }
}

struct Stub {
    client: AxonClient,
    requests: mpsc::UnboundedReceiver<String>,
    abort: AbortHandle,
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

impl Stub {
    async fn new(replies: Vec<Reply>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = AxonClient::new(format!("http://{}", listener.local_addr().unwrap()), None);
        let (tx, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buffer[..n]);
                }
                tx.send(String::from_utf8(request).unwrap()).unwrap();
                tokio::time::sleep(reply.delay).await;
                let response = format!(
                    "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    reply.status, reply.body.len(), reply.body,
                );
                // The client can cancel a page or reject it before EOF.
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        Self {
            client,
            requests,
            abort: task.abort_handle(),
        }
    }

    async fn request(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .unwrap()
            .expect("hierarchy request")
    }
}

fn page(ids: &[String], cursor: Option<&str>, total: u64) -> Value {
    json!({
        "room_id": "!work:srv",
        "children": ids.iter().map(|id| json!({
            "room_id": id, "parent_room_id": "!work:srv", "via": ["srv"],
            // Deliberately unrelated to sidebar membership. The same-account
            // room list, not discovered fields or this flag, owns navigation.
            "joined": false, "discovered": {"name": "Discovery is not a room"}
        })).collect::<Vec<_>>(),
        "next_cursor": cursor, "total_children": total,
        "discovery": {"outcome": "truncated"}
    })
}

fn start(app: &mut App, stub: &Stub) -> mpsc::UnboundedReceiver<SpaceOutcome> {
    app.client = stub.client.clone();
    app.spaces.order_ready = true;
    app.spaces.order_read_again = false;
    let (tx, rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    app.invalidate_space(&key("!work:srv"));
    app.sweep_spaces(Instant::now());
    rx
}

async fn settle(app: &mut App, rx: &mut mpsc::UnboundedReceiver<SpaceOutcome>) {
    let result = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .unwrap()
        .unwrap();
    app.apply_space_outcome(result);
}

#[tokio::test]
async fn pages_beyond_1000_deduplicate_and_keep_sidebar_projection_and_account_scope() {
    let ids: Vec<_> = (0..1105).map(|n| format!("!child{n}:srv")).collect();
    let mut pages = Vec::new();
    // A child moving in MSC1772 order can appear again on a later page.
    let mut with_duplicates = ids.clone();
    with_duplicates.insert(100, ids[0].clone());
    for (n, chunk) in with_duplicates.chunks(HIERARCHY_PAGE_SIZE).enumerate() {
        let cursor = format!("page{}", n + 1);
        pages.push(Reply::page(page(
            chunk,
            (n < 11).then_some(cursor.as_str()),
            ids.len() as u64,
        )));
    }
    let mut stub = Stub::new(pages).await;
    let mut app = app();
    app.rooms
        .rooms
        .extend(ids.iter().map(|id| room(Uuid::nil(), id, id, false)));
    let other = Uuid::new_v4();
    app.rooms
        .rooms
        .push(room(other, &ids[0], "Other account", false));
    // Unjoined IDs returned by hierarchy remain absent from navigation.
    app.rooms.rooms.retain(|r| r.room_id != ids[1]);
    app.spaces.collapsed.insert(key("!club:srv"));
    let saved_order = app.spaces.order.clone();
    let mut rx = start(&mut app, &stub);
    settle(&mut app, &mut rx).await;
    assert_eq!(
        app.spaces.children[&key("!work:srv")]
            .children
            .as_ref()
            .unwrap(),
        &ids
    );
    assert!(app.spaces.children[&key("!work:srv")].error.is_none());
    assert_eq!(app.spaces.collapsed, HashSet::from([key("!club:srv")]));
    assert_eq!(app.spaces.order, saved_order);
    let joined_index = app
        .rooms
        .rooms
        .iter()
        .position(|r| r.account_id.is_nil() && r.room_id == ids[0])
        .unwrap();
    let other_index = app.rooms.rooms.len() - 1;
    assert_eq!(app.room_parent_key(joined_index), Some(key("!work:srv")));
    assert_eq!(app.room_parent_key(other_index), None);
    assert!(!leaves(&app).contains(&ids[1]));
    for n in 0..12 {
        let request = stub.request().await;
        assert!(
            request.starts_with(&format!(
                "GET /v1/accounts/{}/rooms/%21work%3Asrv/space/hierarchy?",
                Uuid::nil()
            )),
            "{request}"
        );
        assert!(request.contains("limit=100&enrich=false"), "{request}");
        assert_eq!(request.contains("cursor="), n != 0);
        if n > 0 {
            assert!(request.contains(&format!("cursor=page{n}")));
        }
    }
}

#[tokio::test]
async fn later_page_failure_retains_grouping_and_retries_from_the_first_page() {
    let mut stub = Stub::new(vec![
        Reply::page(page(&["!c:srv".to_owned()], Some("next"), 2)),
        Reply {
            status: "503 Service Unavailable",
            body: "{}".to_owned(),
            delay: Duration::ZERO,
        },
        Reply::page(page(&["!c:srv".to_owned()], None, 1)),
    ])
    .await;
    let mut app = app();
    let mut rx = start(&mut app, &stub);
    settle(&mut app, &mut rx).await;
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
    assert!(state.error.is_some());
    assert_eq!(state.failures, 1);
    let retry = state.retry_at.unwrap();
    assert!(state.dirty);
    let buffer = render(&mut app, 160, 20);
    assert!(room_text(&app, &buffer).contains("Work (incomplete; retrying)"));
    app.sweep_spaces(retry - Duration::from_secs(1));
    assert!(app.spaces.children[&key("!work:srv")].inflight.is_none());
    app.sweep_spaces(retry);
    settle(&mut app, &mut rx).await;
    assert_eq!(
        app.spaces.children[&key("!work:srv")]
            .children
            .as_ref()
            .unwrap(),
        &["!c:srv"]
    );
    assert!(app.spaces.children[&key("!work:srv")].error.is_none());
    assert_eq!(app.spaces.children[&key("!work:srv")].failures, 0);
    assert!(app.spaces.children[&key("!work:srv")].retry_at.is_none());
    assert!(!stub.request().await.contains("cursor="));
    assert!(stub.request().await.contains("cursor=next"));
    assert!(!stub.request().await.contains("cursor="));
}

#[tokio::test]
async fn malformed_pages_and_cursors_cannot_settle_membership() {
    let mut malformed = Vec::new();
    for cursor in [
        "".to_owned(),
        "x".repeat(HIERARCHY_CURSOR_BYTES + 1),
        "bad\ncursor".to_owned(),
    ] {
        malformed.push(page(&[], Some(&cursor), 1));
    }
    let mut wrong_room = page(&[], None, 0);
    wrong_room["room_id"] = json!("!other:srv");
    malformed.push(wrong_room);
    for (field, value) in [
        ("parent_room_id", json!("!other:srv")),
        ("room_id", json!("invalid")),
        ("via", json!([])),
    ] {
        let mut wrong_child = page(&["!a:srv".to_owned()], None, 1);
        wrong_child["children"][0][field] = value;
        malformed.push(wrong_child);
    }
    malformed.push(page(&[], None, 1));
    malformed.push(page(
        &vec!["!a:srv".to_owned(); HIERARCHY_PAGE_SIZE + 1],
        None,
        1,
    ));
    let mut missing_cursor = page(&[], None, 0);
    missing_cursor
        .as_object_mut()
        .unwrap()
        .remove("next_cursor");
    malformed.push(missing_cursor);
    for value in malformed {
        let stub = Stub::new(vec![Reply::page(value)]).await;
        assert!(walk_space_children(&stub.client, &key("!work:srv"))
            .await
            .is_err());
    }
    let mut stub = Stub::new(
        ["a", "b", "a"]
            .map(|cursor| Reply::page(page(&[], Some(cursor), 1)))
            .into(),
    )
    .await;
    let err = walk_space_children(&stub.client, &key("!work:srv"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("repeated"));
    for _ in 0..3 {
        stub.request().await;
    }
}

#[tokio::test]
async fn page_and_total_body_budgets_apply_before_decoding_ignored_summaries() {
    let mut oversized = page(&[], None, 0);
    oversized["discovery"] = json!({"padding": "x".repeat(HIERARCHY_PAGE_BYTES)});
    let stub = Stub::new(vec![Reply::page(oversized)]).await;
    let err = walk_space_children(&stub.client, &key("!work:srv"))
        .await
        .unwrap_err();
    assert!(matches!(err, WalkFailure::Budget(BudgetKind::PageBytes)));
    assert!(err.to_string().contains("size limit"));

    let replies = (0..9)
        .map(|n| {
            let mut value = page(&[], Some(&format!("next{n}")), 1);
            value["discovery"] = json!({"padding": "x".repeat(HIERARCHY_PAGE_BYTES - 512)});
            Reply::page(value)
        })
        .collect();
    let mut stub = Stub::new(replies).await;
    let err = walk_space_children(&stub.client, &key("!work:srv"))
        .await
        .unwrap_err();
    assert!(matches!(err, WalkFailure::Budget(BudgetKind::PageBytes)));
    assert!(err.to_string().contains("size limit"));
    for _ in 0..9 {
        stub.request().await;
    }
}

#[tokio::test]
async fn exactly_exhausted_walk_budget_stops_before_requesting_another_page() {
    let replies = (0..HIERARCHY_MAX_BYTES / HIERARCHY_PAGE_BYTES)
        .map(|n| {
            let mut value = page(&[], Some(&format!("next{n}")), 1);
            value["padding"] = json!("");
            let mut reply = Reply::page(value);
            let padding = HIERARCHY_PAGE_BYTES - reply.body.len();
            reply.body = reply.body.replace(
                "\"padding\":\"\"",
                &format!("\"padding\":\"{}\"", "x".repeat(padding)),
            );
            assert_eq!(reply.body.len(), HIERARCHY_PAGE_BYTES);
            reply
        })
        .collect();
    let mut stub = Stub::new(replies).await;
    let error = walk_space_children(&stub.client, &key("!work:srv"))
        .await
        .unwrap_err();
    assert!(matches!(error, WalkFailure::Budget(BudgetKind::TotalBytes)));
    assert!(error.to_string().contains("byte budget exhausted"));
    for _ in 0..HIERARCHY_MAX_BYTES / HIERARCHY_PAGE_BYTES {
        stub.request().await;
    }
}

#[tokio::test]
async fn unique_cursors_and_empty_pages_still_have_a_work_limit() {
    let replies = (0..HIERARCHY_MAX_PAGES)
        .map(|n| Reply::page(page(&[], Some(&format!("next{n}")), u64::MAX)))
        .collect();
    let stub = Stub::new(replies).await;
    let err = walk_space_children(&stub.client, &key("!work:srv"))
        .await
        .unwrap_err();
    assert!(matches!(err, WalkFailure::Budget(BudgetKind::Pages)));
    assert!(err.to_string().contains("200 pages"));
}

#[tokio::test]
async fn whole_walk_deadline_bounds_multiple_individually_timely_pages() {
    let replies = (0..3)
        .map(|n| {
            let mut reply = Reply::page(page(&[], Some(&format!("next{n}")), 1));
            reply.delay = Duration::from_secs(25);
            reply
        })
        .collect();
    let mut stub = Stub::new(replies).await;
    let client = stub.client.clone();
    let task = tokio::spawn(async move { walk_space_children(&client, &key("!work:srv")).await });
    stub.request().await;
    tokio::time::pause();
    for _ in 0..2 {
        tokio::time::advance(Duration::from_secs(25)).await;
        stub.request().await;
    }
    tokio::time::advance(Duration::from_secs(10)).await;
    let err = task.await.unwrap().unwrap_err();
    assert!(matches!(err, WalkFailure::Transient(_)));
    assert!(err.to_string().contains("walk deadline"));
}

#[tokio::test]
async fn cancellation_between_pages_releases_permit_and_fences_queued_results() {
    for remove in [false, true] {
        let mut stub = Stub::new(vec![
            Reply::page(page(&["!c:srv".to_owned()], Some("next"), 2)),
            Reply {
                delay: Duration::from_secs(3600),
                ..Reply::page(page(&[], None, 0))
            },
        ])
        .await;
        let mut app = app();
        let mut rx = start(&mut app, &stub);
        let request = app.spaces.children[&key("!work:srv")].inflight.unwrap();
        stub.request().await;
        assert!(stub.request().await.contains("cursor=next"));
        assert_eq!(app.spaces.workers.available_permits(), WORKERS - 1);
        if remove {
            app.rooms.rooms.remove(0);
            app.reconcile_spaces();
            // Rejoin the same key before the old queued result arrives.
            app.rooms
                .rooms
                .insert(0, room(Uuid::nil(), "!work:srv", "Work", true));
            seed(&mut app, "!work:srv", &["!b:srv"]);
        } else {
            app.invalidate_space(&key("!work:srv"));
        }
        tokio::task::yield_now().await;
        assert_eq!(app.spaces.workers.available_permits(), WORKERS);
        assert!(
            rx.try_recv().is_err(),
            "canceled worker must not publish a partial page"
        );
        app.apply_space_outcome(SpaceOutcome::Children {
            key: key("!work:srv"),
            request,
            result: Ok(vec!["!c:srv".to_owned()]),
        });
        assert!(!app.spaces.children[&key("!work:srv")]
            .children
            .as_ref()
            .unwrap()
            .contains(&"!c:srv".to_owned()));
        let fresh = Stub::new(vec![Reply::page(page(&["!c:srv".to_owned()], None, 1))]).await;
        app.client = fresh.client.clone();
        app.invalidate_space(&key("!work:srv"));
        app.sweep_spaces(Instant::now());
        let new_request = app.spaces.children[&key("!work:srv")].inflight.unwrap();
        assert_ne!(new_request, request);
        app.apply_space_outcome(SpaceOutcome::Children {
            key: key("!work:srv"),
            request,
            result: Ok(Vec::new()),
        });
        assert_eq!(
            app.spaces.children[&key("!work:srv")].inflight,
            Some(new_request)
        );
        settle(&mut app, &mut rx).await;
        assert_eq!(
            app.spaces.children[&key("!work:srv")]
                .children
                .as_ref()
                .unwrap(),
            &["!c:srv"]
        );
    }
}

#[tokio::test]
async fn account_change_during_walk_cannot_consume_another_accounts_membership_or_navigation() {
    let mut stub = Stub::new(vec![Reply::page(page(&["!a:srv".to_owned()], None, 1))]).await;
    let mut app = app();
    let mut rx = start(&mut app, &stub);
    stub.request().await;
    select_other_account(&mut app);
    let other = app.active_account_filter().unwrap();
    app.rooms.rooms.extend([
        room(other, "!work:srv", "Other work", true),
        room(other, "!a:srv", "Other Alpha", false),
        room(other, "!b:srv", "Other Beta", false),
    ]);
    let other_key = RoomKey {
        account_id: other,
        room_id: "!work:srv".to_owned(),
    };
    app.spaces.children.insert(
        other_key.clone(),
        ChildrenState {
            children: Some(vec!["!b:srv".to_owned()]),
            ..Default::default()
        },
    );
    let selected = app.rooms.selected;
    app.spaces.navigation = None;
    settle(&mut app, &mut rx).await;
    assert_eq!(app.rooms.selected, selected);
    // The completed cache is keyed to its originating account even though
    // that account is no longer the active filter.
    assert_eq!(
        app.spaces.children[&key("!work:srv")]
            .children
            .as_ref()
            .unwrap(),
        &["!a:srv"]
    );
    assert_eq!(
        app.spaces.children[&other_key].children.as_ref().unwrap(),
        &["!b:srv"]
    );
    assert_eq!(
        app.room_parent_key(app.rooms.rooms.len() - 1),
        Some(other_key)
    );
    assert_eq!(app.room_parent_key(app.rooms.rooms.len() - 2), None);
    assert!(app
        .visible_room_indices()
        .iter()
        .all(|&i| app.rooms.rooms[i].account_id != Uuid::nil()));
}

fn fail_membership(app: &mut App, now: Instant, request: u64, failure: WalkFailure) {
    let id = key("!work:srv");
    let state = app.spaces.children.get_mut(&id).unwrap();
    // A sweep clears the dirty bit when it starts the walk. Leaving it set
    // discards the result, which is the superseded-walk path.
    state.dirty = false;
    state.inflight = Some(request);
    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: id,
            request,
            result: Err(failure),
        },
        now,
    );
}

fn timed_out() -> WalkFailure {
    WalkFailure::Transient(crate::api::ApiError::Request(
        "request timed out".to_owned(),
    ))
}

#[test]
fn transient_hierarchy_failures_double_until_four_minutes_and_success_resets() {
    let now = Instant::now();
    let mut app = app();
    let expected = [30_u64, 60, 120, 240, 240];
    for (step, delay) in expected.into_iter().enumerate() {
        fail_membership(&mut app, now, step as u64 + 1, timed_out());
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, (step as u32 + 1).min(4), "step {step}");
        assert_eq!(state.retry_at, Some(now + Duration::from_secs(delay)));
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
        assert!(state.error.is_some());
        assert!(state.dirty);
    }

    let state = app.spaces.children.get_mut(&key("!work:srv")).unwrap();
    state.dirty = false;
    state.inflight = Some(9);
    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 9,
            result: Ok(vec!["!c:srv".to_owned()]),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 0);
    assert!(state.retry_at.is_none());
    assert!(state.error.is_none());
    assert_eq!(state.children.as_ref().unwrap(), &["!c:srv"]);

    fail_membership(&mut app, now, 10, timed_out());
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 1);
    assert_eq!(state.retry_at, Some(now + Duration::from_secs(30)));
    assert_eq!(state.children.as_ref().unwrap(), &["!c:srv"]);

    app.refresh_spaces();
    assert!(app.spaces.children[&key("!work:srv")].retry_at.is_none());
    assert_eq!(app.spaces.children[&key("!work:srv")].failures, 0);
    fail_membership(&mut app, now, 11, timed_out());
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 1);
    assert_eq!(state.retry_at, Some(now + Duration::from_secs(30)));
    assert_eq!(state.children.as_ref().unwrap(), &["!c:srv"]);
}

#[test]
fn walk_deadline_stays_on_the_transient_ladder() {
    let now = Instant::now();
    let mut app = app();
    for (step, delay) in [30_u64, 60].into_iter().enumerate() {
        fail_membership(
            &mut app,
            now,
            step as u64 + 1,
            WalkFailure::Transient(crate::api::ApiError::Request(
                HIERARCHY_DEADLINE_ERROR.to_owned(),
            )),
        );
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, step as u32 + 1);
        assert_eq!(state.retry_at, Some(now + Duration::from_secs(delay)));
        assert_eq!(state.error.as_deref(), Some(HIERARCHY_DEADLINE_ERROR));
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
    }
}

#[test]
fn budget_hierarchy_failures_start_at_the_cap_and_keep_the_grouping() {
    let now = Instant::now();
    let budgets = [
        BudgetKind::TotalBytes,
        BudgetKind::Pages,
        BudgetKind::PageBytes,
    ];
    for (index, kind) in budgets.into_iter().enumerate() {
        let mut app = app();
        app.display.debug = true;
        let message = WalkFailure::Budget(kind).to_string();
        fail_membership(&mut app, now, 1, WalkFailure::Budget(kind));
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, 4, "{message}");
        assert_eq!(
            state.retry_at,
            Some(now + Duration::from_secs(240)),
            "{message}"
        );
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
        assert_eq!(state.error.as_deref(), Some(message.as_str()));
        let status = app.status.text(true);
        assert!(status.contains(&message), "{status}");
        assert!(status.contains("retry in 240s"), "{status}");
        if index == 0 {
            let buffer = render(&mut app, 160, 20);
            assert!(room_text(&app, &buffer).contains("Work (incomplete; retrying)"));
        }
        fail_membership(&mut app, now, 2, WalkFailure::Budget(kind));
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, 4);
        assert_eq!(state.retry_at, Some(now + Duration::from_secs(240)));
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
    }
}

#[tokio::test]
async fn relationship_changes_keep_a_capped_wait_and_refresh_clears_it() {
    let now = Instant::now();
    let mut app = app();
    app.spaces.order_read_again = false;
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    fail_membership(
        &mut app,
        now,
        1,
        WalkFailure::Budget(BudgetKind::TotalBytes),
    );
    let retry = app.spaces.children[&key("!work:srv")].retry_at.unwrap();
    app.invalidate_space(&key("!work:srv"));
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 4);
    assert_eq!(state.retry_at, Some(retry));
    assert!(state.dirty);
    assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
    app.sweep_spaces(retry - Duration::from_secs(1));
    assert!(app.spaces.children[&key("!work:srv")].inflight.is_none());

    // A child event during the capped attempt lets that attempt finish.
    // It does not abort the worker or start the 240-second wait over.
    let state = app.spaces.children.get_mut(&key("!work:srv")).unwrap();
    let deadline = now - Duration::from_secs(1);
    state.retry_at = Some(deadline);
    state.inflight = Some(9);
    state.dirty = false;
    app.invalidate_space(&key("!work:srv"));
    app.invalidate_space(&key("!work:srv"));
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 4);
    assert_eq!(state.inflight, Some(9));
    assert!(!state.dirty);
    assert_eq!(state.retry_at, Some(deadline));

    // The attempt fails on its own. The child events do not add a second delay.
    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 9,
            result: Err(WalkFailure::Budget(BudgetKind::Pages)),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 4);
    assert_eq!(state.retry_at, Some(now + Duration::from_secs(240)));
    assert!(state.dirty);
    assert!(state.inflight.is_none());

    app.refresh_spaces();
    app.spaces.order_read_again = false;
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 0);
    assert!(state.retry_at.is_none());
    assert!(state.error.is_some());
    app.sweep_spaces(Instant::now());
    assert!(app.spaces.children[&key("!work:srv")].inflight.is_some());
}

#[test]
fn stale_hierarchy_results_do_not_change_failure_history() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(60);
    let mut app = app();
    let other = RoomKey {
        account_id: Uuid::from_u128(7),
        room_id: "!work:srv".to_owned(),
    };
    app.spaces.children.insert(
        other.clone(),
        ChildrenState {
            children: Some(vec!["!b:srv".to_owned()]),
            failures: 2,
            retry_at: Some(deadline),
            ..Default::default()
        },
    );
    let state = app.spaces.children.get_mut(&key("!work:srv")).unwrap();
    state.inflight = Some(9);
    state.failures = 2;
    state.retry_at = Some(deadline);
    state.error = Some("offline".to_owned());
    state.dirty = true;

    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 9,
            result: Ok(Vec::new()),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 2);
    assert_eq!(state.retry_at, Some(deadline));
    assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
    assert!(state.inflight.is_none());
    assert!(state.dirty);

    let state = app.spaces.children.get_mut(&key("!work:srv")).unwrap();
    state.inflight = Some(9);
    state.dirty = false;
    for result in [Err(timed_out()), Ok(vec!["!c:srv".to_owned()])] {
        app.apply_space_outcome_at(
            SpaceOutcome::Children {
                key: key("!work:srv"),
                request: 8,
                result,
            },
            now,
        );
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, 2);
        assert_eq!(state.retry_at, Some(deadline));
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
        assert_eq!(state.inflight, Some(9));
    }

    app.invalidate_space(&key("!work:srv"));
    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 9,
            result: Err(WalkFailure::Transient(crate::api::ApiError::Request(
                HIERARCHY_DEADLINE_ERROR.to_owned(),
            ))),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 2);
    assert!(state.retry_at.is_none());
    assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
    assert_eq!(state.error.as_deref(), Some("offline"));
    assert_eq!(app.spaces.children[&other].failures, 2);
    assert_eq!(app.spaces.children[&other].retry_at, Some(deadline));
}

#[test]
fn removing_a_root_drops_hierarchy_failure_history() {
    let now = Instant::now();
    let mut app = app();
    fail_membership(&mut app, now, 1, timed_out());
    assert_eq!(app.spaces.children[&key("!work:srv")].failures, 1);
    app.rooms.rooms.remove(0);
    app.reconcile_spaces();
    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 1,
            result: Err(WalkFailure::Budget(BudgetKind::Pages)),
        },
        now,
    );
    assert!(!app.spaces.children.contains_key(&key("!work:srv")));

    app.rooms
        .rooms
        .insert(0, room(Uuid::nil(), "!work:srv", "Work", true));
    seed(&mut app, "!work:srv", &["!a:srv"]);
    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 1,
            result: Err(timed_out()),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 0);
    assert!(state.retry_at.is_none());
    assert_eq!(state.children.as_ref().unwrap(), &["!a:srv"]);
}

#[tokio::test]
async fn explicit_refresh_resets_backoff_and_a_relationship_change_retries_below_the_cap() {
    for refresh in [true, false] {
        let mut stub = Stub::new(vec![
            Reply {
                status: "503 Service Unavailable",
                body: "{}".to_owned(),
                delay: Duration::ZERO,
            },
            Reply::page(page(&["!c:srv".to_owned()], None, 1)),
        ])
        .await;
        let mut app = app();
        // One root, so the retry is not raced by another space's walk.
        app.rooms.rooms.retain(|room| room.room_id != "!club:srv");
        app.reconcile_spaces();
        let mut rx = start(&mut app, &stub);
        settle(&mut app, &mut rx).await;
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, 1);
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
        let retry = state.retry_at.unwrap();
        if refresh {
            app.refresh_spaces();
            // `/refresh` also re-reads space order. That request is not this
            // stub's hierarchy page, so keep the next connection for the walk.
            app.spaces.order_read_again = false;
        } else {
            app.invalidate_space(&key("!work:srv"));
        }
        let state = &app.spaces.children[&key("!work:srv")];
        assert!(state.retry_at.is_none());
        // `/refresh` starts the ladder over. A relationship change below the
        // cap retries immediately and keeps the count.
        assert_eq!(state.failures, if refresh { 0 } else { 1 });
        assert!(state.error.is_some());
        assert_eq!(state.children.as_ref().unwrap(), &["!a:srv", "!b:srv"]);
        app.sweep_spaces(retry - Duration::from_secs(1));
        settle(&mut app, &mut rx).await;
        let state = &app.spaces.children[&key("!work:srv")];
        assert_eq!(state.failures, 0);
        assert!(state.retry_at.is_none());
        assert!(state.error.is_none());
        assert_eq!(state.children.as_ref().unwrap(), &["!c:srv"]);
        let first = stub.request().await;
        assert!(!first.contains("cursor="), "{first}");
        let second = stub.request().await;
        assert!(!second.contains("cursor="), "{second}");
    }
}

#[tokio::test]
async fn a_capped_walk_that_succeeds_after_a_child_event_walks_once_more() {
    let now = Instant::now();
    let mut app = app();
    app.spaces.order_read_again = false;
    let (tx, _rx) = mpsc::unbounded_channel();
    app.spaces.tx = Some(tx);
    fail_membership(
        &mut app,
        now,
        1,
        WalkFailure::Budget(BudgetKind::TotalBytes),
    );
    let state = app.spaces.children.get_mut(&key("!work:srv")).unwrap();
    state.retry_at = Some(now - Duration::from_secs(1));
    state.inflight = Some(9);
    state.dirty = false;
    app.invalidate_space(&key("!work:srv"));
    app.invalidate_space(&key("!work:srv"));

    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: 9,
            result: Ok(vec!["!c:srv".to_owned()]),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 0);
    assert!(state.retry_at.is_none());
    assert!(state.dirty);
    assert!(state.inflight.is_none());
    assert!(state.error.is_none());
    assert_eq!(state.children.as_ref().unwrap(), &["!c:srv"]);

    app.sweep_spaces(now);
    let follow = app.spaces.children[&key("!work:srv")].inflight;
    assert!(follow.is_some());
    app.sweep_spaces(now);
    assert_eq!(app.spaces.children[&key("!work:srv")].inflight, follow);

    app.apply_space_outcome_at(
        SpaceOutcome::Children {
            key: key("!work:srv"),
            request: follow.unwrap(),
            result: Ok(vec!["!d:srv".to_owned()]),
        },
        now,
    );
    let state = &app.spaces.children[&key("!work:srv")];
    assert_eq!(state.failures, 0);
    assert!(!state.dirty);
    assert!(state.inflight.is_none());
    assert!(state.retry_at.is_none());
    assert_eq!(state.children.as_ref().unwrap(), &["!d:srv"]);
}
