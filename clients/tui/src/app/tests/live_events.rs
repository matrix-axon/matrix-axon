//! Live timeline frames: which room they resolve to, what they advance, and which
//! ones are hidden from the timeline entirely.

use super::support::*;
use crate::app::timeline::RedactionReconcile;
use crate::app::*;

#[test]
fn live_event_for_unknown_room_requests_room_refresh() {
    let mut app = app_with_rooms(Vec::new());
    let event = event_with_id(
        "$new:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(action, LiveFrameAction::RefreshRooms);
}

/// ADR 0089, live path. A backfilled event delivered while the room is on
/// screen is older by `origin_ts` than the marker already holds, so the
/// marker correctly refuses it — but it arrived last, so it is exactly what
/// the receipt must name. The receipt has to advance on its own.
#[test]
fn live_event_advances_the_receipt_target_when_the_marker_refuses() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    app.rooms.selected = Some(0);
    let key = RoomKey {
        account_id: Uuid::nil(),
        room_id: "!room:example.com".to_owned(),
    };
    app.read_markers.insert(
        key.clone(),
        read_markers::ReadMarker {
            event_id: "$bridge".to_owned(),
            origin_ts: 1_785_928_309_453,
        },
    );

    let mut event = event_with_id(
        "$backfilled:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );
    event.origin_ts = 1_785_928_304_987;
    event.arrival_order = 1_871_426;
    app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(app.read_markers.get(&key).unwrap().event_id, "$bridge");
    assert_eq!(
        app.receipt_targets.get(&key).unwrap().event_id,
        "$backfilled:example.com"
    );
}

/// The `event_shown` gate governs both halves: an event the user cannot see
/// is neither a marker position nor a receipt target.
#[test]
fn hidden_live_event_advances_neither_position() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    app.rooms.selected = Some(0);
    let key = RoomKey {
        account_id: Uuid::nil(),
        room_id: "!room:example.com".to_owned(),
    };

    let mut event = event_with_id(
        "$reaction:example.com",
        "m.reaction",
        None,
        serde_json::json!({
            "m.relates_to": { "rel_type": "m.annotation", "event_id": "$t", "key": "👍" }
        }),
    );
    event.origin_ts = 5_000;
    event.arrival_order = 9_999;
    app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert!(!app.read_markers.contains_key(&key));
    assert!(!app.receipt_targets.contains_key(&key));
}

#[test]
fn live_event_for_known_unselected_room_does_not_touch_the_room_badge() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    let event = event_with_id(
        "$known:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(action, LiveFrameAction::None);
    // The room badge is the server's count (ADR 0115), not a live increment.
    assert_eq!(
        app.rooms
            .unread
            .get(&RoomKey {
                account_id: Uuid::nil(),
                room_id: "!room:example.com".to_owned(),
            })
            .copied(),
        None
    );
}

#[test]
fn own_live_leave_event_for_selected_room_requests_room_refresh() {
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = Some("@me:example.com".to_owned());
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let mut event = event_with_state_key(
        "$leave:example.com",
        "m.room.member",
        Some("@me:example.com"),
        None,
        serde_json::json!({ "membership": "leave" }),
    );
    event.room_id = room.room_id.clone();

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(action, LiveFrameAction::RefreshRooms);
}

#[test]
fn own_live_ban_event_for_unselected_room_requests_room_refresh() {
    let mut target = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    target.account_user_id = Some("@me:example.com".to_owned());
    let mut other = room(
        "!other:example.com",
        Some("#other:example.com"),
        Some("Other"),
    );
    other.account_user_id = Some("@me:example.com".to_owned());
    let mut app = app_with_rooms(vec![other, target.clone()]);
    app.rooms.selected = Some(0);
    let mut event = event_with_state_key(
        "$ban:example.com",
        "m.room.member",
        Some("@me:example.com"),
        None,
        serde_json::json!({ "membership": "ban" }),
    );
    event.room_id = target.room_id.clone();

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(action, LiveFrameAction::RefreshRooms);
}

#[test]
fn peer_live_leave_event_for_known_room_does_not_request_room_refresh() {
    let mut room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    room.account_user_id = Some("@me:example.com".to_owned());
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let mut event = event_with_state_key(
        "$leave:example.com",
        "m.room.member",
        Some("@peer:example.com"),
        None,
        serde_json::json!({ "membership": "leave" }),
    );
    event.room_id = room.room_id.clone();

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(action, LiveFrameAction::None);
}

#[test]
fn live_formatted_edit_replaces_rendered_content() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let mut original = event_with_id(
        "$original:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({
            "msgtype": "m.text",
            "body": "hello",
            "format": "org.matrix.custom.html",
            "formatted_body": "<em>hello</em>"
        }),
    );
    original.room_id = room.room_id.clone();
    app.messages
        .events
        .insert(RoomKey::from(&room), vec![original]);

    let mut edit = event_with_id(
        "$edit:example.com",
        "m.room.message",
        Some("* hello world"),
        serde_json::json!({
            "msgtype": "m.text",
            "body": "* hello world",
            "m.new_content": {
                "msgtype": "m.text",
                "body": "hello world",
                "format": "org.matrix.custom.html",
                "formatted_body": "<strong>hello world</strong>"
            }
        }),
    );
    edit.room_id = room.room_id.clone();
    edit.relates_to = Some(serde_json::json!({
        "rel_type": "m.replace",
        "event_id": "$original:example.com"
    }));

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(edit)));

    assert_eq!(action, LiveFrameAction::None);
    let updated = &app.messages.events[&RoomKey::from(&room)][0];
    assert_eq!(updated.body.as_deref(), Some("hello world"));
    assert_eq!(
        updated.formatted_body(),
        Some("<strong>hello world</strong>")
    );
}

#[test]
fn hidden_live_event_for_known_unselected_room_does_not_update_unread() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    let event = event_with_id(
        "$reaction:example.com",
        "m.reaction",
        None,
        serde_json::json!({
            "m.relates_to": {
                "rel_type": "m.annotation",
                "event_id": "$known:example.com",
                "key": "👍"
            }
        }),
    );

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(event)));

    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(
        app.rooms.unread.get(&RoomKey {
            account_id: Uuid::nil(),
            room_id: "!room:example.com".to_owned(),
        }),
        None
    );
}

fn room_key() -> RoomKey {
    RoomKey {
        account_id: Uuid::nil(),
        room_id: "!room:example.com".to_owned(),
    }
}

fn counts_frame(room_id: &str, notification_count: u64) -> LiveFrame {
    LiveFrame::UnreadCounts {
        account_id: Uuid::nil(),
        payload: crate::api::UnreadCountsChangedDto {
            room_id: room_id.to_owned(),
            notification_count,
            highlight_count: 0,
        },
    }
}

#[test]
fn non_message_live_events_leave_the_room_badge_alone() {
    let membership = event_with_state_key(
        "$join:example.com",
        "m.room.member",
        Some("@bob:example.com"),
        None,
        serde_json::json!({ "membership": "join" }),
    );
    let mut edit = event_with_id(
        "$edit:example.com",
        "m.room.message",
        Some("* hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "* hello" }),
    );
    edit.relates_to = Some(serde_json::json!({
        "rel_type": "m.replace",
        "event_id": "$known:example.com"
    }));
    for show_state_events in [false, true] {
        for event in [&membership, &edit] {
            let mut app = app_with_rooms(vec![room(
                "!room:example.com",
                Some("#room:example.com"),
                Some("Room"),
            )]);
            app.display.show_state_events = show_state_events;
            app.handle_live_frame(LiveFrame::Timeline(Box::new(event.clone())));
            assert_eq!(app.rooms.unread.get(&room_key()), None);
        }
    }
}

#[test]
fn unread_counts_frame_sets_and_clears_the_room_badge() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);

    let action = app.handle_live_frame(counts_frame("!room:example.com", 4));
    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(app.rooms.unread.get(&room_key()).copied(), Some(4));

    app.handle_live_frame(counts_frame("!room:example.com", 0));
    assert_eq!(app.rooms.unread.get(&room_key()), None);
}

#[test]
fn unread_counts_frame_for_unknown_room_requests_a_refresh() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    let action = app.handle_live_frame(counts_frame("!elsewhere:example.com", 2));
    assert_eq!(action, LiveFrameAction::RefreshRooms);
}

#[test]
fn unread_counts_frame_does_not_badge_the_selected_room() {
    let mut app = app_with_rooms(vec![room(
        "!room:example.com",
        Some("#room:example.com"),
        Some("Room"),
    )]);
    app.rooms.selected = Some(0);
    app.handle_live_frame(counts_frame("!room:example.com", 3));
    assert_eq!(app.rooms.unread.get(&room_key()), None);
}

#[test]
fn room_refresh_seeds_badges_from_notification_count() {
    let mut unread = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    unread.notification_count = 7;
    let mut app = app_with_rooms(vec![unread.clone()]);
    app.apply_room_refresh(vec![unread.clone()]);
    assert_eq!(app.rooms.unread.get(&room_key()).copied(), Some(7));

    // A later refresh with the count at zero clears it.
    unread.notification_count = 0;
    app.apply_room_refresh(vec![unread]);
    assert_eq!(app.rooms.unread.get(&room_key()), None);
}

#[test]
fn room_refresh_does_not_overwrite_a_badge_a_frame_set_meanwhile() {
    // The first listed room is the one a refresh selects, and a selected room
    // is never badged, so the subject is the second.
    let open = room("!open:example.com", None, Some("Open"));
    let stale = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![open.clone(), stale.clone()]);

    // A frame lands while an older list fetch is still in flight.
    app.handle_live_frame(counts_frame("!room:example.com", 4));
    // The older response carries the count from before the frame.
    app.apply_room_refresh(vec![open.clone(), stale.clone()]);
    assert_eq!(app.rooms.unread.get(&room_key()).copied(), Some(4));

    // The guard covers one apply; the next fetch is authoritative again.
    let mut fresh = stale;
    fresh.notification_count = 6;
    app.apply_room_refresh(vec![open, fresh]);
    assert_eq!(app.rooms.unread.get(&room_key()).copied(), Some(6));
}

#[test]
fn room_refresh_does_not_bring_back_a_locally_cleared_badge() {
    let open = room("!open:example.com", None, Some("Open"));
    let mut listed = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    listed.notification_count = 5;
    let mut app = app_with_rooms(vec![open.clone(), listed.clone()]);
    app.apply_room_refresh(vec![open.clone(), listed.clone()]);
    assert_eq!(app.rooms.unread.get(&room_key()).copied(), Some(5));

    // The user read the room; a response fetched before that still says 5.
    app.clear_room_unread(&room_key());
    app.apply_room_refresh(vec![open, listed]);
    assert_eq!(app.rooms.unread.get(&room_key()), None);
}

#[test]
pub(crate) fn find_room_matches_incomplete_alias_localpart() {
    let app = app_with_rooms(vec![room(
        "!abc:example.com",
        Some("#test:example.com"),
        Some("Test Room"),
    )]);

    assert_eq!(
        app.resolve_room_target("test"),
        RoomTargetResolution::Match(0)
    );
    assert_eq!(
        app.resolve_room_target("#test"),
        RoomTargetResolution::Match(0)
    );
    assert_eq!(
        app.resolve_room_target("TEST"),
        RoomTargetResolution::Match(0)
    );
}

#[test]
pub(crate) fn find_room_matches_one_based_room_list_number() {
    let app = app_with_rooms(vec![
        room("!one:example.com", Some("#one:example.com"), Some("One")),
        room("!two:example.com", Some("#two:example.com"), Some("Two")),
    ]);

    assert_eq!(app.resolve_room_target("1"), RoomTargetResolution::Match(0));
    assert_eq!(app.resolve_room_target("2"), RoomTargetResolution::Match(1));
    assert_eq!(app.resolve_room_target("0"), RoomTargetResolution::Missing);
    assert_eq!(app.resolve_room_target("3"), RoomTargetResolution::Missing);
}

#[test]
fn room_resolution_ignores_rooms_hidden_by_account_filter() {
    let visible_account = Uuid::from_u128(1);
    let hidden_account = Uuid::from_u128(2);
    let mut visible = room("!visible:example.com", None, Some("General"));
    visible.account_id = visible_account;
    let mut hidden = room("!hidden:example.com", None, Some("General"));
    hidden.account_id = hidden_account;
    let mut app = app_with_rooms(vec![visible, hidden]);
    app.set_accounts(vec![
        account_with_id(
            visible_account,
            "@visible:example.com",
            AccountState::Active,
        ),
        account_with_id(hidden_account, "@hidden:example.com", AccountState::Active),
    ]);
    app.accounts.selected = AccountSelection::Account(0);

    assert_eq!(
        app.resolve_room_target("General"),
        RoomTargetResolution::Match(0)
    );
}

#[test]
pub(crate) fn relative_room_index_wraps_next_and_previous() {
    assert_eq!(relative_room_index(0, 3, 1), 1);
    assert_eq!(relative_room_index(2, 3, 1), 0);
    assert_eq!(relative_room_index(1, 3, -1), 0);
    assert_eq!(relative_room_index(0, 3, -1), 2);
}

#[test]
fn event_filter_hides_state_events_but_keeps_membership() {
    let mut display = DisplayOptions {
        debug: false,
        show_state_events: false,
        message_density: MessageDensity::Normal,
        time_format: TimeFormat::H24,
        input_lines: 1,
        max_input_lines: None,
        preview_warmup_count: 5,
        highlight_selected_line: false,
        confirm_logout: true,
        search_wrap: true,
        accept_incoming_verification: true,
        accounts_panel_width: 25,
        rooms_panel_width_adj: 0,
        pinned_rooms: Vec::new(),
        room_sort: "recent".to_owned(),
        room_filter: "all".to_owned(),
    };
    let state = event_with_state_key(
        "$m.room.topic:example.com",
        "m.room.topic",
        Some(""),
        None,
        serde_json::json!({ "topic": "new topic" }),
    );
    let membership = event_with_state_key(
        "$m.room.member:example.com",
        "m.room.member",
        Some("@alice:example.com"),
        None,
        serde_json::json!({ "membership": "join" }),
    );
    let message = event(
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );
    let utd = EventDto {
        content: None,
        body: None,
        event_type: "m.room.encrypted".to_owned(),
        ..event("m.room.encrypted", None, serde_json::json!({}))
    };

    assert!(!should_show_event(&state, &display));
    assert!(should_show_event(&membership, &display));
    assert!(should_show_event(&message, &display));
    assert!(should_show_event(&utd, &display));

    display.show_state_events = true;
    assert!(should_show_event(&state, &display));
}

#[test]
pub(crate) fn sender_label_defaults_to_membership_display_name() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let membership = EventDto {
        sender: "@jamie:example.com".to_owned(),
        ..event_with_state_key(
            "$member:example.com",
            "m.room.member",
            Some("@alice:example.com"),
            None,
            serde_json::json!({
                "membership": "join",
                "displayname": "Alice"
            }),
        )
    };
    app.rebuild_display_names(&room, &[membership]);
    let message = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );

    assert_eq!(app.sender_label(&message), "Alice");
}

#[test]
pub(crate) fn sender_label_prefers_display_name_in_both_densities() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let membership = event_with_state_key(
        "$member:example.com",
        "m.room.member",
        Some("@alice:example.com"),
        None,
        serde_json::json!({
            "membership": "join",
            "displayname": "Alice"
        }),
    );
    app.rebuild_display_names(&room, &[membership]);
    let message = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );

    // A known display name is shown regardless of layout density.
    app.display.message_density = MessageDensity::Normal;
    assert_eq!(app.sender_label(&message), "Alice");
    app.display.message_density = MessageDensity::Dense;
    assert_eq!(app.sender_label(&message), "Alice");
}

#[test]
pub(crate) fn incremental_history_keeps_existing_display_names() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.seed_display_names_from_members(
        &room,
        &[MemberDto {
            user_id: "@alice:example.com".to_owned(),
            membership: "join".to_owned(),
            display_name: Some("Current Alice".to_owned()),
        }],
    );
    let stale_membership = event_with_state_key(
        "$older-member:example.com",
        "m.room.member",
        Some("@alice:example.com"),
        None,
        serde_json::json!({
            "membership": "join",
            "displayname": "Old Alice"
        }),
    );

    app.merge_missing_display_names_from_events(&room, &[stale_membership]);

    let message = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );
    assert_eq!(app.sender_label(&message), "Current Alice");
}

#[test]
pub(crate) fn sender_label_without_display_name_varies_by_density() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    // No membership event, so no display name is known for the sender.
    let message = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );

    // Normal mode shows the full mxid; dense mode drops the homeserver and
    // keeps the `@localpart`.
    app.display.message_density = MessageDensity::Normal;
    assert_eq!(app.sender_label(&message), "@alice:example.com");
    app.display.message_density = MessageDensity::Dense;
    assert_eq!(app.sender_label(&message), "@alice");
}

#[test]
pub(crate) fn members_outcome_resolves_previously_unknown_sender() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.display.message_density = MessageDensity::Normal;
    let message = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );
    // No name known yet — falls back to the raw mxid.
    assert_eq!(app.sender_label(&message), "@alice:example.com");

    // A background /members refresh lands and resolves the name in place.
    app.apply_members_outcome(timeline::MembersOutcome {
        room_key: RoomKey::from(&room),
        members: Some(vec![MemberDto {
            user_id: "@alice:example.com".to_owned(),
            membership: "join".to_owned(),
            display_name: Some("Alice".to_owned()),
        }]),
    });
    assert_eq!(app.sender_label(&message), "Alice");
}

#[test]
pub(crate) fn members_refresh_is_skipped_within_cooldown() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    let (tx, _rx) = mpsc::unbounded_channel();
    app.set_members_sender(tx);
    let key = RoomKey::from(&room);
    // Pre-arm a future cooldown deadline. A refresh inside the window must be
    // skipped at the gate — if it fell through to tokio::spawn it would panic
    // outside a runtime, and the deadline would be overwritten.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    app.members_refresh_after.insert(key.clone(), deadline);
    app.spawn_members_refresh(key.clone());
    assert_eq!(app.members_refresh_after.get(&key), Some(&deadline));
}

fn selected_room_app() -> (App, RoomKey) {
    let listed = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let key = RoomKey::from(&listed);
    let mut app = app_with_rooms(vec![listed]);
    app.rooms.selected = Some(0);
    (app, key)
}

fn text_message(event_id: &str, body: &str) -> EventDto {
    event_with_id(
        event_id,
        "m.room.message",
        Some(body),
        serde_json::json!({
            "msgtype": "m.text",
            "body": body,
            "format": "org.matrix.custom.html",
            "formatted_body": "<em>hello</em>"
        }),
    )
}

#[test]
fn live_redaction_of_a_loaded_message_is_a_tombstone_and_moves_nothing() {
    let (mut app, key) = selected_room_app();
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    app.read_markers.insert(
        key.clone(),
        read_markers::ReadMarker {
            event_id: "$bridge".to_owned(),
            origin_ts: 100,
        },
    );
    app.receipt_targets.insert(
        key.clone(),
        read_markers::ReceiptTarget {
            event_id: "$bridge".to_owned(),
            arrival_order: 5,
        },
    );

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$msg:example.com",
    ))));

    assert_eq!(action, LiveFrameAction::None);
    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, "$msg:example.com");
    assert!(events[0].redacted);
    assert_eq!(
        events[0].redaction_event_id.as_deref(),
        Some("$redact:example.com")
    );
    assert!(events[0].body.is_none());
    assert!(events[0].content.is_none());
    assert!(events[0].formatted_body().is_none());
    assert_eq!(events[0].display_body(), "[redacted]");
    assert_eq!(
        app.selected_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect::<Vec<_>>(),
        vec!["$msg:example.com"]
    );
    assert_eq!(app.read_markers[&key].event_id, "$bridge");
    assert_eq!(app.receipt_targets[&key].event_id, "$bridge");
}

#[test]
fn live_redaction_of_a_loaded_image_drops_the_thumbnail() {
    let (mut app, key) = selected_room_app();
    let image = event_with_id(
        "$img:example.com",
        "m.room.message",
        Some("photo.jpg"),
        serde_json::json!({
            "msgtype": "m.image",
            "body": "photo.jpg",
            "url": "mxc://example.com/abc"
        }),
    );
    assert!(image.image_mxc().is_some());
    app.messages.events.insert(key.clone(), vec![image]);

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$img:example.com",
    ))));

    let updated = &app.messages.events[&key][0];
    assert!(updated.redacted);
    assert!(updated.image_mxc().is_none());
    assert!(updated.body.is_none());
    assert!(updated.content.is_none());
    assert_eq!(updated.display_body(), "[redacted]");
    assert_eq!(
        updated.redaction_event_id.as_deref(),
        Some("$redact:example.com")
    );
}

#[test]
fn live_redaction_during_a_history_jump_masks_without_inserting() {
    let (mut app, key) = selected_room_app();
    app.last_jump_ts = Some(1);
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$msg:example.com",
    ))));

    assert_eq!(action, LiveFrameAction::None);
    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert!(events[0].redacted);
    assert_eq!(events[0].display_body(), "[redacted]");
}

#[test]
fn a_redaction_whose_target_is_not_loaded_appends_nothing() {
    let (mut app, key) = selected_room_app();
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$missing:example.com",
    ))));

    assert_eq!(action, LiveFrameAction::None);
    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert!(!events[0].redacted);
    assert_eq!(events[0].body.as_deref(), Some("hello"));
    assert!(app.stale_redactions.contains_key(&key));

    // A message for an unknown room asks for a refresh. A redaction must not.
    let mut unknown = app_with_rooms(Vec::new());
    let action = unknown.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$missing:example.com",
    ))));
    assert_eq!(action, LiveFrameAction::None);
    assert!(unknown.messages.events.is_empty());
}

#[test]
fn a_redaction_with_no_target_appends_nothing() {
    let (mut app, key) = selected_room_app();
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    let mut frame = event_with_id(
        "$redact:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );
    frame.sender = "@bob:example.com".to_owned();

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));

    assert_eq!(action, LiveFrameAction::None);
    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert!(!events[0].redacted);
    assert_eq!(events[0].body.as_deref(), Some("hello"));
    assert!(app.stale_redactions.contains_key(&key));
}

#[test]
fn a_second_redaction_of_the_same_message_is_a_no_op() {
    let (mut app, key) = selected_room_app();
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    let first = redaction_frame("$redact:example.com", "$msg:example.com");
    app.handle_live_frame(LiveFrame::Timeline(Box::new(first.clone())));
    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(first)));
    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$again:example.com",
        "$msg:example.com",
    ))));

    assert_eq!(action, LiveFrameAction::None);
    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert!(events[0].redacted);
    assert_eq!(
        events[0].redaction_event_id.as_deref(),
        Some("$redact:example.com")
    );
}

#[test]
fn a_redaction_is_not_a_visible_row_and_leaves_the_room_badge() {
    let (mut app, key) = selected_room_app();
    app.display.show_state_events = true;
    app.rooms.unread.insert(key.clone(), 3);
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$msg:example.com",
    ))));

    assert_eq!(action, LiveFrameAction::None);
    assert_eq!(app.rooms.unread.get(&key).copied(), Some(3));
    assert!(app.messages.events[&key]
        .iter()
        .all(|event| event.event_id != "$redact:example.com"));
    assert!(app
        .selected_events()
        .iter()
        .all(|event| event.event_id != "$redact:example.com"));
}

#[test]
fn a_live_redaction_rewrites_an_in_slice_reply_snippet() {
    let (mut app, key) = selected_room_app();
    let original = text_message("$orig:example.com", "hello world");
    let mut reply = event_with_id(
        "$reply:example.com",
        "m.room.message",
        Some("re"),
        serde_json::json!({ "msgtype": "m.text", "body": "re" }),
    );
    reply.relates_to = Some(serde_json::json!({
        "m.in_reply_to": { "event_id": "$orig:example.com" }
    }));
    app.reply_targets.insert(
        (Uuid::nil(), "$orig:example.com".to_owned()),
        original.clone(),
    );
    app.messages
        .events
        .insert(key.clone(), vec![original, reply]);

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$orig:example.com",
    ))));

    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    let preview = ctx.replies.get("$reply:example.com").expect("preview");
    assert_eq!(preview.snippet, "[redacted]");
    let cached = &app.reply_targets[&(Uuid::nil(), "$orig:example.com".to_owned())];
    assert!(cached.redacted);
    assert_eq!(cached.display_body(), "[redacted]");
    assert!(cached.body.is_none());
}

#[test]
fn a_live_redaction_rewrites_a_cached_reply_when_the_target_is_off_slice() {
    let (mut app, key) = selected_room_app();
    let original = text_message("$orig:example.com", "hello world");
    let mut reply = event_with_id(
        "$reply:example.com",
        "m.room.message",
        Some("re"),
        serde_json::json!({ "msgtype": "m.text", "body": "re" }),
    );
    reply.relates_to = Some(serde_json::json!({
        "m.in_reply_to": { "event_id": "$orig:example.com" }
    }));
    app.reply_targets
        .insert((Uuid::nil(), "$orig:example.com".to_owned()), original);
    app.messages.events.insert(key, vec![reply]);

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$orig:example.com",
    ))));

    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    let preview = ctx.replies.get("$reply:example.com").expect("preview");
    assert_eq!(preview.snippet, "[redacted]");
}

fn edit_event(event_id: &str, target: &str, body: &str) -> EventDto {
    let mut event = event_with_id(
        event_id,
        "m.room.message",
        Some(&format!("* {body}")),
        serde_json::json!({
            "msgtype": "m.text",
            "body": format!("* {body}"),
            "m.new_content": { "msgtype": "m.text", "body": body }
        }),
    );
    event.relates_to = Some(serde_json::json!({
        "rel_type": "m.replace",
        "event_id": target
    }));
    event
}

fn search_holding(event: &EventDto) -> crate::search::SearchResultsState {
    let key = crate::search::SearchContextKey::from_event(event);
    crate::search::SearchResultsState {
        request: crate::search::SearchRequest {
            q: "hello".to_owned(),
            account_id: None,
            room_id: None,
            sender: None,
            from: None,
            to: None,
            limit: crate::search::DEFAULT_SEARCH_LIMIT,
            sort: crate::search::SearchSortOrder::NewestFirst,
            cursor: None,
        },
        edit_form: crate::search::SearchFormState::default(),
        results: vec![crate::api::SearchResultDto {
            event: event.clone(),
            score: 1.0,
        }],
        total: 1,
        next_cursor: None,
        selected: 0,
        loading: false,
        grouping: crate::search::SearchGrouping::None,
        context_cache: std::collections::HashMap::from([(
            key,
            crate::search::SearchResultContext {
                events: vec![event.clone()],
            },
        )]),
    }
}

#[test]
fn a_redaction_with_no_target_does_not_blank_the_entry_line() {
    let (mut app, key) = selected_room_app();
    app.display.debug = false;
    app.status = Status::from("ready");
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    let frame = event_with_id(
        "$redact:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));

    assert_eq!(app.status.text(false), "ready");
    assert!(app.stale_redactions.contains_key(&key));
    assert_eq!(app.messages.events[&key][0].body.as_deref(), Some("hello"));
}

#[test]
fn a_redaction_with_no_target_logs_when_debug_display_is_on() {
    let (mut app, key) = selected_room_app();
    app.display.debug = true;
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    let frame = event_with_id(
        "$redact:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));

    assert_eq!(
        app.status.text(true),
        "live redaction has no content.redacts; the loaded timeline is stale until the next read"
    );
    assert!(app.stale_redactions.contains_key(&key));
}

#[test]
fn a_redaction_with_no_target_stays_quiet_mid_command() {
    let (mut app, key) = selected_room_app();
    app.display.debug = true;
    app.mode = Mode::Editing {
        event_id: "$msg:example.com".to_owned(),
    };
    app.status = Status::from("editing");
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    let frame = event_with_id(
        "$redact:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));

    assert_eq!(app.status.text(true), "editing");
    assert!(app.stale_redactions.contains_key(&key));
}

#[test]
fn a_page_heal_masks_a_server_redacted_message_and_keeps_a_local_mask() {
    let (mut app, key) = selected_room_app();
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$msg:example.com", "hello")]);
    let frame = event_with_id(
        "$redact:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );
    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));
    app.mask_redacted_message(&key, "$msg:example.com", "$local:example.com");

    let mut server = text_message("$msg:example.com", "hello again");
    let extra = text_message("$other:example.com", "nope");
    app.last_jump_ts = Some(1);
    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![server.clone(), extra]),
        epoch: 0,
    });

    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1, "a heal must not append rows");
    assert!(
        events[0].redacted,
        "an unredacted snapshot must not clear a local mask"
    );
    assert!(events[0].body.is_none());
    assert_eq!(
        events[0].redaction_event_id.as_deref(),
        Some("$local:example.com")
    );

    // The mask above already consumed the mark's page. A fresh unnamed
    // redaction of a still-visible body is covered by the next read.
    app.messages.events.insert(
        key.clone(),
        vec![text_message("$visible:example.com", "still here")],
    );
    let frame = event_with_id(
        "$redact-2:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );
    app.last_jump_ts = None;
    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));
    server = text_message("$visible:example.com", "still here");
    server.redacted = true;
    server.body = None;
    server.content = None;
    server.redaction_event_id = Some("$server:example.com".to_owned());
    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![server]),
        epoch: 0,
    });

    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert!(events[0].redacted);
    assert_eq!(events[0].display_body(), "[redacted]");
    assert!(!app.stale_redactions.contains_key(&key));
}

#[test]
fn redacting_a_loaded_edit_discards_the_row_and_restores_the_original() {
    let (mut app, key) = selected_room_app();
    let original = text_message("$orig:example.com", "edited");
    let edit = edit_event("$edit:example.com", "$orig:example.com", "edited");
    app.search_results = Some(search_holding(&edit));
    app.messages
        .events
        .insert(key.clone(), vec![original, edit]);
    app.last_jump_ts = Some(1);

    let action = app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$edit:example.com",
    ))));

    assert_eq!(action, LiveFrameAction::None);
    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id, "$orig:example.com");
    assert!(!events[0].redacted);
    assert_eq!(events[0].body.as_deref(), Some("edited"));
    assert!(app.stale_redactions.contains_key(&key));
    let hit = &app.search_results.as_ref().unwrap().results[0].event;
    assert!(hit.redacted);
    assert!(hit.body.is_none());
    assert!(app
        .search_results
        .as_ref()
        .unwrap()
        .context_cache
        .values()
        .all(|context| context
            .events
            .iter()
            .all(|event| event.event_id != "$edit:example.com")));

    let restored = text_message("$orig:example.com", "hello");
    app.apply_redaction_reconcile(RedactionReconcile::Event {
        room_key: key.clone(),
        event_id: "$orig:example.com".to_owned(),
        event: Some(Box::new(restored)),
        epoch: 0,
    });

    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1);
    assert!(!events[0].redacted);
    assert_eq!(events[0].body.as_deref(), Some("hello"));
    assert!(!app.stale_redactions.contains_key(&key));
}

#[test]
fn a_folded_edit_redaction_copies_the_server_body_onto_the_original() {
    let (mut app, key) = selected_room_app();
    app.messages.events.insert(
        key.clone(),
        vec![text_message("$orig:example.com", "edited")],
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$edit:example.com",
    ))));
    assert_eq!(app.messages.events[&key][0].body.as_deref(), Some("edited"));
    assert!(app.stale_redactions.contains_key(&key));

    let healed = text_message("$orig:example.com", "hello");
    let untouched = text_message("$older:example.com", "older");
    app.messages.events.get_mut(&key).unwrap().push(untouched);
    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![healed]),
        epoch: 0,
    });

    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].body.as_deref(), Some("hello"));
    assert!(!events[0].redacted);
    assert_eq!(events[1].body.as_deref(), Some("older"));
    assert!(!app.stale_redactions.contains_key(&key));
}

/// A heal page is newest-first and still carries `m.replace` rows. Folding
/// them before the copy keeps a message that was not the redaction's target.
#[test]
fn an_unnamed_redaction_keeps_an_unrelated_edit() {
    let (mut app, key) = selected_room_app();
    app.messages.events.insert(
        key.clone(),
        vec![text_message("$kept:example.com", "second edit")],
    );
    let frame = event_with_id(
        "$redact:example.com",
        "m.room.redaction",
        None,
        serde_json::json!({}),
    );
    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));

    let mut original = text_message("$kept:example.com", "original");
    original.origin_ts = 1;
    let mut older = edit_event("$edit-old:example.com", "$kept:example.com", "first edit");
    older.origin_ts = 2;
    let mut newer = edit_event("$edit-new:example.com", "$kept:example.com", "second edit");
    newer.origin_ts = 3;
    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![newer, older, original]),
        epoch: 0,
    });

    let events = &app.messages.events[&key];
    assert_eq!(events.len(), 1, "edit rows are not appended");
    assert_eq!(events[0].body.as_deref(), Some("second edit"));
    assert!(!app.stale_redactions.contains_key(&key));
}

#[test]
fn an_unloaded_target_outside_the_open_room_does_not_mark_it_stale() {
    let (mut app, key) = selected_room_app();
    app.messages
        .events
        .insert(key.clone(), vec![text_message("$open:example.com", "open")]);
    let other = RoomKey {
        account_id: key.account_id,
        room_id: "!other:example.com".to_owned(),
    };
    let mut hidden = text_message("$hidden:example.com", "edited");
    hidden.room_id = other.room_id.clone();
    app.messages.events.insert(other.clone(), vec![hidden]);
    let mut frame = redaction_frame("$redact:example.com", "$edit:example.com");
    frame.room_id = other.room_id.clone();

    app.handle_live_frame(LiveFrame::Timeline(Box::new(frame)));

    assert!(!app.stale_redactions.contains_key(&other));
    assert!(!app.stale_redactions.contains_key(&key));
    assert_eq!(
        app.messages.events[&other][0].body.as_deref(),
        Some("edited")
    );
}

#[test]
fn a_page_from_a_cleared_heal_does_not_overwrite_a_newer_mark() {
    let (mut app, key) = selected_room_app();
    app.messages.events.insert(
        key.clone(),
        vec![text_message("$msg:example.com", "edited")],
    );
    let unnamed = || {
        event_with_id(
            "$redact:example.com",
            "m.room.redaction",
            None,
            serde_json::json!({}),
        )
    };
    app.handle_live_frame(LiveFrame::Timeline(Box::new(unnamed())));
    app.clear_stale_redaction(&key);
    app.handle_live_frame(LiveFrame::Timeline(Box::new(unnamed())));
    assert!(app.stale_redactions.contains_key(&key));

    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![text_message("$msg:example.com", "original")]),
        epoch: 0,
    });
    assert_eq!(app.messages.events[&key][0].body.as_deref(), Some("edited"));
    assert!(app.stale_redactions.contains_key(&key));

    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![text_message("$msg:example.com", "edited")]),
        epoch: 1,
    });
    assert_eq!(app.messages.events[&key][0].body.as_deref(), Some("edited"));
    assert!(!app.stale_redactions.contains_key(&key));
}

#[test]
fn a_search_jump_drops_an_in_flight_redaction_heal() {
    let (mut app, key) = selected_room_app();
    app.messages.events.insert(
        key.clone(),
        vec![text_message("$kept:example.com", "edited")],
    );
    let unnamed = || {
        event_with_id(
            "$redact:example.com",
            "m.room.redaction",
            None,
            serde_json::json!({}),
        )
    };
    app.handle_live_frame(LiveFrame::Timeline(Box::new(unnamed())));
    assert!(app.stale_redactions.contains_key(&key));

    let jumped = text_message("$kept:example.com", "from the jump");
    app.apply_timeline_around_search_event(
        jumped.clone(),
        Ok(TimelinePage {
            events: vec![jumped],
            next_cursor: None,
        }),
        None,
    );
    assert_eq!(
        app.messages.events[&key][0].body.as_deref(),
        Some("from the jump")
    );
    assert!(!app.stale_redactions.contains_key(&key));

    // A redaction after the jump starts a new heal. The read that was already
    // in flight belongs to the previous epoch and must not clear this one.
    app.handle_live_frame(LiveFrame::Timeline(Box::new(unnamed())));
    assert!(app.stale_redactions.contains_key(&key));
    app.apply_redaction_reconcile(RedactionReconcile::Page {
        room_key: key.clone(),
        events: Some(vec![text_message("$kept:example.com", "original")]),
        epoch: 0,
    });
    assert_eq!(
        app.messages.events[&key][0].body.as_deref(),
        Some("from the jump")
    );
    assert!(app.stale_redactions.contains_key(&key));
}

#[test]
fn a_live_redaction_masks_search_copies_of_the_target() {
    let (mut app, key) = selected_room_app();
    let message = text_message("$msg:example.com", "hello");
    let mut other_account = message.clone();
    other_account.account_id = Uuid::from_u128(1);
    let mut search = search_holding(&message);
    search.results.push(crate::api::SearchResultDto {
        event: other_account,
        score: 1.0,
    });
    app.search_results = Some(search);
    app.messages.events.insert(key.clone(), vec![message]);

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact:example.com",
        "$msg:example.com",
    ))));

    let search = app.search_results.as_ref().unwrap();
    assert!(search.results[0].event.redacted);
    assert!(search.results[0].event.body.is_none());
    assert!(!search.results[1].event.redacted);
    assert_eq!(search.results[1].event.body.as_deref(), Some("hello"));
    let context = search.context_cache.values().next().unwrap();
    assert!(context.events[0].redacted);
    assert!(context.events[0].relates_to.is_none());
}

#[test]
fn merge_redaction_keeps_a_local_mask_and_applies_an_incoming_one() {
    let mut existing = text_message("$msg:example.com", "hello");
    let mut incoming = text_message("$msg:example.com", "hello");
    incoming.redacted = true;
    incoming.body = None;
    incoming.content = None;
    incoming.redaction_event_id = Some("$redact:example.com".to_owned());

    EventDto::merge_redaction(&mut existing, &incoming);
    assert!(existing.redacted);
    assert!(existing.body.is_none());
    assert_eq!(
        existing.redaction_event_id.as_deref(),
        Some("$redact:example.com")
    );

    let stale = text_message("$msg:example.com", "hello");
    EventDto::merge_redaction(&mut existing, &stale);
    assert!(existing.redacted);
    assert!(existing.body.is_none());
}

#[test]
fn live_redaction_of_a_thread_reply_decrements_the_summary_once() {
    let (mut app, key) = selected_room_app();
    let root = text_message("$root:example.com", "root");
    let mut first = text_message("$m1:example.com", "first");
    first.relates_to = Some(serde_json::json!({
        "rel_type": "m.thread",
        "event_id": "$root:example.com"
    }));
    let mut second = text_message("$m2:example.com", "second");
    second.relates_to = Some(serde_json::json!({
        "rel_type": "m.thread",
        "event_id": "$root:example.com"
    }));
    app.messages
        .events
        .insert(key.clone(), vec![root, first, second]);
    app.thread_summaries.insert(
        key.clone(),
        std::collections::HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 2,
            },
        )]),
    );

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact-1:example.com",
        "$m1:example.com",
    ))));
    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact-1-again:example.com",
        "$m1:example.com",
    ))));

    let member = app.messages.events[&key]
        .iter()
        .find(|event| event.event_id == "$m1:example.com")
        .unwrap();
    assert!(member.redacted);
    assert!(member.relates_to.is_some(), "a read keeps relates_to");
    assert_eq!(
        app.thread_summaries[&key]["$root:example.com"].reply_count,
        1
    );
    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    assert_eq!(ctx.thread_badges.get("$root:example.com").unwrap().count, 1);

    app.handle_live_frame(LiveFrame::Timeline(Box::new(redaction_frame(
        "$redact-2:example.com",
        "$m2:example.com",
    ))));

    assert!(!app.thread_summaries[&key].contains_key("$root:example.com"));
    let events = app.selected_events();
    let ctx = app.relation_context(&events);
    assert!(!ctx.thread_badges.contains_key("$root:example.com"));
}
