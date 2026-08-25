//! Replies and threads: reply context resolution, thread badges, the unread-thread
//! picker, and the thread panel.

use super::support::*;
use crate::app::*;

#[tokio::test]
async fn reply_and_thread_actions_target_selected_message() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("message"),
            serde_json::json!({ "msgtype": "m.text", "body": "message" }),
        )],
    );
    app.messages.selection = Some("$message:example.com".to_owned());

    app.start_reply_to_selected_message();
    assert_eq!(app.pending_reply.as_deref(), Some("$message:example.com"));
    assert_eq!(app.pending_thread, None);

    // A standalone message heads no thread, so /thread composes a new thread
    // rooted at it (ADR 0032 M4) rather than opening a panel.
    app.start_thread_from_selected_message().await;
    assert_eq!(app.pending_thread.as_deref(), Some("$message:example.com"));
    assert_eq!(app.pending_reply, None);
}

fn reply_event(event_id: &str, target: &str) -> EventDto {
    let mut event = event_with_id(
        event_id,
        "m.room.message",
        Some("reply body"),
        serde_json::json!({ "msgtype": "m.text", "body": "reply body" }),
    );
    event.relates_to = Some(serde_json::json!({
        "m.in_reply_to": { "event_id": target }
    }));
    event
}

fn thread_event(event_id: &str, root: &str, body: &str) -> EventDto {
    let mut event = event_with_id(
        event_id,
        "m.room.message",
        Some(body),
        serde_json::json!({ "msgtype": "m.text", "body": body }),
    );
    event.relates_to = Some(serde_json::json!({
        "rel_type": "m.thread",
        "event_id": root
    }));
    event
}

fn ids(events: &[&EventDto]) -> Vec<String> {
    events.iter().map(|event| event.event_id.clone()).collect()
}

fn unread_thread(root: &str, count: usize, latest_ts: i64) -> UnreadThread {
    UnreadThread {
        root_event_id: root.to_owned(),
        unread_count: count,
        latest_event_id: format!("{root}-reply"),
        latest_sender: "@bob:example.com".to_owned(),
        latest_body: "new reply".to_owned(),
        latest_ts,
        recent: vec![UnreadThreadPreview {
            event_id: format!("{root}-reply"),
            sender: "@bob:example.com".to_owned(),
            body: "new reply".to_owned(),
            origin_ts: latest_ts,
        }],
        counted: std::collections::HashSet::from([format!("{root}-reply")]),
    }
}

#[test]
fn reply_context_resolves_from_loaded_slice() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let original = event_with_id(
        "$orig:example.com",
        "m.room.message",
        Some("hello world"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello world" }),
    );
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            original,
            reply_event("$reply:example.com", "$orig:example.com"),
        ],
    );

    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    let preview = ctx.replies.get("$reply:example.com").expect("preview");
    assert_eq!(preview.sender, "@alice:example.com");
    assert_eq!(preview.snippet, "hello world");
}

#[test]
fn reply_context_absent_when_target_off_slice() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![reply_event("$reply:example.com", "$missing:example.com")],
    );

    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    // No resolved preview => the layout renders the placeholder line.
    assert!(ctx.replies.is_empty());
}

#[test]
fn thread_members_are_hidden_from_main_timeline_and_badged_on_root() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            root,
            thread_event("$m1:example.com", "$root:example.com", "first"),
            thread_event("$m2:example.com", "$root:example.com", "second"),
        ],
    );

    // Main timeline shows only the root.
    assert_eq!(ids(&app.selected_events()), vec!["$root:example.com"]);

    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    let badge = ctx.thread_badges.get("$root:example.com").expect("badge");
    assert_eq!(badge.count, 2);
    assert_eq!(badge.latest_sender.as_deref(), Some("@alice:example.com"));
    assert_eq!(badge.latest_snippet.as_deref(), Some("second"));
}

#[test]
fn thread_badge_count_prefers_server_aggregate() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            root,
            thread_event("$m1:example.com", "$root:example.com", "first"),
        ],
    );
    app.thread_summaries.insert(
        RoomKey::from(&room),
        HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 7,
            },
        )]),
    );

    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    // Seven total on the server even though only one member is in the slice.
    assert_eq!(ctx.thread_badges.get("$root:example.com").unwrap().count, 7);
}

#[test]
fn stale_relation_outcome_cannot_replace_newer_thread_summary() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let key = RoomKey::from(&room);
    app.relation_refresh_latest.insert(key.clone(), 2);

    app.apply_relation_outcome(relations::RelationOutcome {
        room_key: key.clone(),
        refresh_id: 1,
        account_id: room.account_id,
        threads: Some(HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 99,
            },
        )])),
        replies: Vec::new(),
        is_incremental: false,
    });

    assert!(!app.thread_summaries.contains_key(&key));

    app.apply_relation_outcome(relations::RelationOutcome {
        room_key: key.clone(),
        refresh_id: 2,
        account_id: room.account_id,
        threads: Some(HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 3,
            },
        )])),
        replies: Vec::new(),
        is_incremental: false,
    });

    assert_eq!(
        app.thread_summaries[&key]["$root:example.com"].reply_count,
        3
    );
}

#[test]
fn live_thread_member_increments_cached_server_summary() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(key.clone(), vec![root]);
    app.thread_summaries.insert(
        key.clone(),
        HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 2,
            },
        )]),
    );

    let live = thread_event("$m3:example.com", "$root:example.com", "third");
    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(
        app.thread_summaries[&key]["$root:example.com"].reply_count,
        3
    );
    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    assert_eq!(ctx.thread_badges.get("$root:example.com").unwrap().count, 3);
}

#[test]
fn live_thread_member_marks_thread_unread_when_panel_closed() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(key.clone(), vec![root]);

    let mut live = thread_event("$reply:example.com", "$root:example.com", "new reply");
    live.sender = "@bob:example.com".to_owned();
    app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

    assert_eq!(
        app.unread_threads[&key]["$root:example.com"].unread_count,
        1
    );
    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    assert_eq!(ctx.thread_badges["$root:example.com"].unread_count, 1);
    assert_eq!(app.rooms.unread.get(&key), None);
}

#[test]
fn live_thread_member_for_unselected_room_marks_room_and_thread_unread() {
    let unread_room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let other = room(
        "!other:example.com",
        Some("#other:example.com"),
        Some("Other"),
    );
    let mut app = app_with_rooms(vec![unread_room.clone(), other]);
    app.rooms.selected = Some(1);
    let key = RoomKey::from(&unread_room);

    let mut live = thread_event("$reply:example.com", "$root:example.com", "new reply");
    live.sender = "@bob:example.com".to_owned();
    app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

    assert_eq!(app.rooms.unread.get(&key).copied(), Some(1));
    assert_eq!(
        app.unread_threads[&key]["$root:example.com"].unread_count,
        1
    );
}

#[test]
fn own_live_thread_member_does_not_mark_thread_unread() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(key.clone(), vec![root]);
    app.seed_own_senders_from_rooms();

    let live = thread_event("$reply:example.com", "$root:example.com", "my reply");
    app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

    assert!(!app.unread_threads.contains_key(&key));
}

#[test]
fn clearing_thread_unread_removes_only_that_thread() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    app.unread_threads.insert(
        key.clone(),
        HashMap::from([
            (
                "$root:example.com".to_owned(),
                unread_thread("$root:example.com", 2, 2),
            ),
            (
                "$other-root:example.com".to_owned(),
                unread_thread("$other-root:example.com", 1, 1),
            ),
        ]),
    );

    app.clear_unread_thread(&key, "$root:example.com");

    assert!(!app.unread_threads[&key].contains_key("$root:example.com"));
    assert!(app.unread_threads[&key].contains_key("$other-root:example.com"));
}

#[test]
fn unread_thread_entries_sort_newest_first_and_include_context() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let key = RoomKey::from(&room);
    app.messages.events.insert(
        key.clone(),
        vec![event_with_id(
            "$root:example.com",
            "m.room.message",
            Some("Root topic"),
            serde_json::json!({ "msgtype": "m.text", "body": "Root topic" }),
        )],
    );
    app.unread_threads.insert(
        key,
        HashMap::from([(
            "$root:example.com".to_owned(),
            unread_thread("$root:example.com", 2, 2),
        )]),
    );

    let entries = app.unread_thread_entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].room_title, "Room");
    assert_eq!(entries[0].root_snippet.as_deref(), Some("Root topic"));
    assert_eq!(entries[0].unread_count, 2);
    assert_eq!(entries[0].recent.len(), 1);
}

#[test]
fn unread_thread_previews_keep_three_newest_posts() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let key = RoomKey::from(&room);

    for idx in 0..5 {
        let mut event = thread_event(
            &format!("$reply{idx}:example.com"),
            "$root:example.com",
            &format!("reply {idx}"),
        );
        event.sender = format!("@sender{idx}:example.com");
        event.origin_ts = idx;
        app.mark_thread_unread_from_event(&key, "$root:example.com", &event);
    }

    let thread = &app.unread_threads[&key]["$root:example.com"];

    assert_eq!(thread.unread_count, 5);
    assert_eq!(
        thread
            .recent
            .iter()
            .map(|preview| preview.body.as_str())
            .collect::<Vec<_>>(),
        vec!["reply 4", "reply 3", "reply 2"]
    );
}

#[test]
fn unread_threads_command_opens_picker_when_entries_exist() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let key = RoomKey::from(&room);
    app.unread_threads.insert(
        key,
        HashMap::from([(
            "$root:example.com".to_owned(),
            unread_thread("$root:example.com", 1, 1),
        )]),
    );

    app.open_unread_threads_picker();

    assert_eq!(app.mode, Mode::Popup(PopupKind::UnreadThreads));
    assert_eq!(app.unread_thread_selection, 0);
}

#[test]
fn unread_thread_selection_follows_identity_after_resort() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let key = RoomKey::from(&room);
    app.unread_threads.insert(
        key.clone(),
        HashMap::from([
            (
                "$root-a:example.com".to_owned(),
                unread_thread("$root-a:example.com", 1, 20),
            ),
            (
                "$root-b:example.com".to_owned(),
                unread_thread("$root-b:example.com", 1, 10),
            ),
        ]),
    );

    app.open_unread_threads_picker();
    assert_eq!(app.unread_thread_selection, 0);
    assert_eq!(
        app.unread_thread_selected
            .as_ref()
            .map(|selected| selected.root_event_id.as_str()),
        Some("$root-a:example.com")
    );

    let mut newer = thread_event("$reply-b:example.com", "$root-b:example.com", "newer");
    newer.sender = "@bob:example.com".to_owned();
    newer.origin_ts = 30;
    app.mark_thread_unread_from_event(&key, "$root-b:example.com", &newer);
    let entries = app.unread_thread_entries();
    app.sync_unread_thread_selection(&entries);

    assert_eq!(entries[0].root_event_id, "$root-b:example.com");
    assert_eq!(app.unread_thread_selection, 1);
    assert_eq!(
        app.unread_thread_selected
            .as_ref()
            .map(|selected| selected.root_event_id.as_str()),
        Some("$root-a:example.com")
    );
}

#[test]
fn unread_threads_command_reports_empty_state_without_popup() {
    let mut app = app_with_rooms(Vec::new());

    app.open_unread_threads_picker();

    assert_eq!(app.mode, Mode::Compose);
    assert_eq!(app.status.text(false), "no unread threads");
}

#[test]
fn live_thread_member_promoted_to_main_timeline_when_panel_closed() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(key.clone(), vec![root]);

    // Live thread member arrives with no thread panel open.
    let live = thread_event("$reply:example.com", "$root:example.com", "my reply");
    app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));

    // The promoted set must contain the new event.
    assert!(app.promoted_thread_events.contains("$reply:example.com"));

    // selected_events() must surface the promoted event in the main timeline.
    let events = app.selected_events();
    let ids = ids(&events);
    assert!(
        ids.contains(&"$reply:example.com".to_owned()),
        "promoted thread member should appear in main timeline"
    );

    // The relation context must carry a thread_context entry for the member.
    let ctx = app.relation_context(&events);
    assert!(
        ctx.thread_contexts.contains_key("$reply:example.com"),
        "thread context should be built for the promoted event"
    );
    // Root is in the slice, so the context resolves (Some(preview)).
    assert!(
        ctx.thread_contexts["$reply:example.com"].is_some(),
        "thread context should resolve when root is in the slice"
    );
}

#[test]
fn promoted_events_cleared_when_thread_panel_opens() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(key.clone(), vec![root]);

    // Promote a live reply.
    let live = thread_event("$reply:example.com", "$root:example.com", "my reply");
    app.handle_live_frame(LiveFrame::Timeline(Box::new(live)));
    assert!(app.promoted_thread_events.contains("$reply:example.com"));

    // Opening the thread panel for that root should clear the promotion.
    app.promoted_thread_events.retain(|id| {
        app.messages.events.get(&key).is_none_or(|events| {
            events
                .iter()
                .find(|e| &e.event_id == id)
                .and_then(|e| e.thread_relation())
                != Some("$root:example.com")
        })
    });
    assert!(
        !app.promoted_thread_events.contains("$reply:example.com"),
        "promoted event should be cleared when panel opens for its root"
    );
}

#[test]
fn thread_panel_shows_root_and_members_then_closes() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            root,
            thread_event("$m1:example.com", "$root:example.com", "first"),
            thread_event("$m2:example.com", "$root:example.com", "second"),
        ],
    );

    app.thread_panel = Some("$root:example.com".to_owned());
    assert_eq!(
        ids(&app.selected_events()),
        vec!["$root:example.com", "$m1:example.com", "$m2:example.com"]
    );

    // Inside the panel the root is labeled and no badge clutters the view.
    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    assert_eq!(ctx.thread_root.as_deref(), Some("$root:example.com"));
    assert!(ctx.thread_badges.is_empty());

    assert!(app.close_thread_panel());
    assert!(app.thread_panel.is_none());
    // After closing the panel, the thread root message should be selected.
    assert_eq!(app.messages.selection.as_deref(), Some("$root:example.com"));
    // Idempotent: a second Esc in the main timeline is not consumed here.
    assert!(!app.close_thread_panel());
}

#[test]
fn search_jump_merges_fetched_thread_before_opening_panel() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);

    let mut root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    root.origin_ts = 100;
    let mut hit = thread_event("$hit:example.com", "$root:example.com", "search hit");
    hit.origin_ts = 200;
    let mut fetched_member =
        thread_event("$fetched:example.com", "$root:example.com", "older context");
    fetched_member.origin_ts = 300;

    app.handle_search_outcome(SearchOutcome::Jump {
        hit: hit.clone(),
        action: SearchJumpAction::View,
        room_refresh: None,
        result: Ok(TimelinePage {
            events: vec![hit],
            next_cursor: None,
        }),
        thread_load: Some(Box::new(SearchJumpThreadLoad {
            timeline: Ok(TimelinePage {
                events: vec![fetched_member],
                next_cursor: None,
            }),
            root_event: Ok(root),
        })),
    });

    assert_eq!(app.thread_panel.as_deref(), Some("$root:example.com"));
    assert_eq!(app.messages.selection.as_deref(), Some("$hit:example.com"));
    assert_eq!(
        ids(&app.selected_events()),
        vec![
            "$root:example.com",
            "$hit:example.com",
            "$fetched:example.com"
        ]
    );
}

#[test]
fn selecting_a_thread_root_hints_the_open_thread_shortcut() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("Let's discuss"),
        serde_json::json!({ "msgtype": "m.text", "body": "Let's discuss" }),
    );
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            root,
            thread_event("$m1:example.com", "$root:example.com", "first"),
        ],
    );

    app.move_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$root:example.com"));
    assert!(
        app.status.text(false).contains("open thread"),
        "status should hint the thread shortcut: {:?}",
        app.status.text(false)
    );
}

#[test]
fn is_thread_root_detects_members_and_server_summary() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let plain = event_with_id(
        "$plain:example.com",
        "m.room.message",
        Some("nothing"),
        serde_json::json!({ "msgtype": "m.text", "body": "nothing" }),
    );
    let root = event_with_id(
        "$root:example.com",
        "m.room.message",
        Some("root"),
        serde_json::json!({ "msgtype": "m.text", "body": "root" }),
    );
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            plain,
            root,
            thread_event("$m1:example.com", "$root:example.com", "first"),
        ],
    );

    assert!(app.is_thread_root("$root:example.com"));
    assert!(!app.is_thread_root("$plain:example.com"));
}
