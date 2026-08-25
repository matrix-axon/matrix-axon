//! Live timeline frames: which room they resolve to, what they advance, and which
//! ones are hidden from the timeline entirely.

use super::support::*;
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
fn live_event_for_known_unselected_room_only_updates_unread() {
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
    assert_eq!(
        app.rooms
            .unread
            .get(&RoomKey {
                account_id: Uuid::nil(),
                room_id: "!room:example.com".to_owned(),
            })
            .copied(),
        Some(1)
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
