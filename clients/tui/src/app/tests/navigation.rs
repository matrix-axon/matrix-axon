//! Moving the message selection through the rendered timeline.

use super::support::*;
use crate::app::*;

#[test]
fn message_navigation_uses_rendered_image_ranges() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.page_size = 3;
    app.messages.scroll = 0;
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$image:example.com",
                "m.room.message",
                Some("caption"),
                serde_json::json!({
                    "msgtype": "m.image",
                    "body": "caption",
                    "filename": "photo.jpg",
                    "url": "mxc://example.com/photo"
                }),
            ),
            event_with_id(
                "$next:example.com",
                "m.room.message",
                Some("next"),
                serde_json::json!({ "msgtype": "m.text", "body": "next" }),
            ),
        ],
    );
    app.messages.selection = Some("$next:example.com".to_owned());
    app.ensure_message_index_visible(1);

    // Nav now measures against the same cached layout `draw` renders, so
    // the property to assert is the one that matters — the target message
    // is fully on screen — rather than a hand-seeded offset. An image
    // inflates its own range, so a nav path using un-inflated ranges lands
    // short and fails here.
    let range = app.cached_message_ranges()[1].clone();
    let page = app.messages.page_size;
    assert!(
        range.start >= app.messages.scroll && range.end <= app.messages.scroll + page,
        "message 1 ({range:?}) must be visible in scroll {}..{}",
        app.messages.scroll,
        app.messages.scroll + page,
    );
}

#[test]
fn message_navigation_selects_displayed_messages() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$one:example.com",
                "m.room.message",
                Some("one"),
                serde_json::json!({ "msgtype": "m.text", "body": "one" }),
            ),
            event_with_id(
                "$two:example.com",
                "m.room.message",
                Some("two"),
                serde_json::json!({ "msgtype": "m.text", "body": "two" }),
            ),
        ],
    );

    app.move_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$one:example.com"));
    app.move_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    app.move_selected_message(-1);
    assert_eq!(app.selected_message_id(), Some("$one:example.com"));
}

#[test]
fn message_navigation_clamps_at_list_edges() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$one:example.com",
                "m.room.message",
                Some("one"),
                serde_json::json!({ "msgtype": "m.text", "body": "one" }),
            ),
            event_with_id(
                "$two:example.com",
                "m.room.message",
                Some("two"),
                serde_json::json!({ "msgtype": "m.text", "body": "two" }),
            ),
        ],
    );

    app.move_selected_message(-1);
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    app.move_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    app.move_selected_message(-10);
    assert_eq!(app.selected_message_id(), Some("$one:example.com"));
}

#[test]
fn message_navigation_moves_by_message_not_display_line() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    // Pin dense layout so the asserted scroll offsets match its line math.
    app.display.message_density = MessageDensity::Dense;
    app.messages.page_size = 2;
    app.messages.width = 80;
    app.messages.scroll = 0;
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$multi:example.com",
                "m.room.message",
                Some("one\ntwo\nthree"),
                serde_json::json!({ "msgtype": "m.text", "body": "one\ntwo\nthree" }),
            ),
            event_with_id(
                "$next:example.com",
                "m.room.message",
                Some("next"),
                serde_json::json!({ "msgtype": "m.text", "body": "next" }),
            ),
        ],
    );

    app.move_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$multi:example.com"));
    app.move_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$next:example.com"));
    assert_eq!(app.messages.scroll, 3);
    app.move_selected_message(-1);
    assert_eq!(app.selected_message_id(), Some("$multi:example.com"));
    assert_eq!(app.messages.scroll, 1);
}

#[test]
fn message_page_navigation_uses_message_pane_page_size() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    // Pin dense layout so the asserted scroll offsets match its line math.
    app.display.message_density = MessageDensity::Dense;
    app.messages.page_size = 3;
    app.messages.scroll = 0;
    app.messages.events.insert(
        RoomKey::from(&room),
        (0..8)
            .map(|index| {
                event_with_id(
                    &format!("${index}:example.com"),
                    "m.room.message",
                    Some("message"),
                    serde_json::json!({ "msgtype": "m.text", "body": "message" }),
                )
            })
            .collect(),
    );

    app.page_selected_message(1);
    assert_eq!(app.selected_message_id(), Some("$3:example.com"));
    assert_eq!(app.messages.scroll, 4);
    app.page_selected_message(-1);
    assert_eq!(app.selected_message_id(), Some("$0:example.com"));
    assert_eq!(app.messages.scroll, 1);
}

#[test]
fn message_navigation_ignores_hidden_state_events() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_state_key(
                "$topic:example.com",
                "m.room.topic",
                Some(""),
                None,
                serde_json::json!({ "topic": "new topic" }),
            ),
            event_with_id(
                "$message:example.com",
                "m.room.message",
                Some("message"),
                serde_json::json!({ "msgtype": "m.text", "body": "message" }),
            ),
        ],
    );

    app.move_selected_message(1);

    assert_eq!(app.selected_message_id(), Some("$message:example.com"));
}
