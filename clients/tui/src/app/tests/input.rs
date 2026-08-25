//! Compose-line editing, and the media preview popup and image protocol encoding
//! its hotkey drives.

use super::support::*;
use crate::app::*;

#[tokio::test]
async fn clear_input_shortcut_aborts_message_selection() {
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
    app.input.buffer = "/room room".to_owned();
    app.input.cursor = app.input.buffer.len();

    app.handle_key(KeyEvent::from(KeyCode::Esc)).await;

    assert_eq!(app.selected_message_id(), None);
    assert_eq!(app.input.buffer, "");
    assert_eq!(app.input.cursor, 0);
}

#[tokio::test]
async fn input_cursor_supports_readline_start_and_end() {
    let mut app = app_with_rooms(Vec::new());
    for ch in "abc".chars() {
        app.handle_key(KeyEvent::from(KeyCode::Char(ch))).await;
    }

    app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
        .await;
    app.handle_key(KeyEvent::from(KeyCode::Char('X'))).await;
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
        .await;
    app.handle_key(KeyEvent::from(KeyCode::Char('Y'))).await;

    assert_eq!(app.input.buffer, "XabcY");
    assert_eq!(app.input.cursor, app.input.buffer.len());
}

#[tokio::test]
async fn arrow_up_from_compose_enters_message_list_mode() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![
            event_with_id(
                "$one:example.com",
                "m.room.message",
                Some("first"),
                serde_json::json!({ "msgtype": "m.text", "body": "first" }),
            ),
            event_with_id(
                "$two:example.com",
                "m.room.message",
                Some("second"),
                serde_json::json!({ "msgtype": "m.text", "body": "second" }),
            ),
        ],
    );

    // Up from no selection: select the last message; switches to MessageList, input untouched
    app.handle_key(KeyEvent::from(KeyCode::Up)).await;
    assert_eq!(app.input.buffer, "");
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    assert!(matches!(app.mode, Mode::MessageList));

    // Up again (now in MessageList): move to the previous message
    app.handle_key(KeyEvent::from(KeyCode::Up)).await;
    assert_eq!(app.input.buffer, "");
    assert_eq!(app.selected_message_id(), Some("$one:example.com"));
    assert!(matches!(app.mode, Mode::MessageList));

    // Up at the first message: stay put
    app.handle_key(KeyEvent::from(KeyCode::Up)).await;
    assert_eq!(app.selected_message_id(), Some("$one:example.com"));

    // Down: move forward
    app.handle_key(KeyEvent::from(KeyCode::Down)).await;
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    assert!(matches!(app.mode, Mode::MessageList));

    // Down at the last message: stay put (Esc returns to Compose)
    app.handle_key(KeyEvent::from(KeyCode::Down)).await;
    assert_eq!(app.selected_message_id(), Some("$two:example.com"));
    assert!(matches!(app.mode, Mode::MessageList));
}

#[tokio::test]
async fn arrow_up_with_no_messages_enters_message_list() {
    let mut app = app_with_rooms(Vec::new());
    app.handle_key(KeyEvent::from(KeyCode::Up)).await;
    assert_eq!(app.input.buffer, "");
    assert!(matches!(app.mode, Mode::MessageList));
}

#[tokio::test]
async fn media_preview_hotkey_opens_popup_for_selected_image() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.selection = Some("$image:example.com".to_owned());
    app.mode = Mode::MessageList;
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$image:example.com",
            "m.room.message",
            Some("photo.jpg"),
            serde_json::json!({
                "msgtype": "m.image",
                "body": "photo.jpg",
                "url": "mxc://example.com/photo"
            }),
        )],
    );

    app.handle_key(KeyEvent::from(KeyCode::Char('v'))).await;

    assert_eq!(app.mode, Mode::Popup(PopupKind::MediaPreview));
}

fn ready_image() -> Arc<image::DynamicImage> {
    Arc::new(image::DynamicImage::new_rgb8(1, 1))
}

/// #51: the outcomes of a protocol request are named, and the two that are
/// faults are counted rather than dropped in silence.
#[test]
fn request_protocol_reports_why_it_did_not_start_an_encode() {
    let mut app = app_with_rooms(Vec::new());
    let key = MediaKey::new(Uuid::nil(), "mxc://example.com/a".to_owned());

    // Degenerate geometry: nothing to encode into.
    assert_eq!(
        app.request_protocol(key.clone(), Size::new(0, 4)),
        ProtocolRequest::EmptySize
    );

    // The image has not been decoded yet. Expected, self-correcting, and
    // deliberately not counted as a drop.
    assert_eq!(
        app.request_protocol(key.clone(), Size::new(8, 4)),
        ProtocolRequest::ImageNotReady
    );
    assert_eq!(app.protocol_drops, ProtocolDropCounts::default());

    // Image ready but the media channel was never wired: every encode for
    // the life of the process dies here, so it is counted.
    app.image_cache
        .insert(key.clone(), ImageState::Ready(ready_image()));
    assert_eq!(
        app.request_protocol(key.clone(), Size::new(8, 4)),
        ProtocolRequest::ChannelUnwired
    );
    assert_eq!(app.protocol_drops.channel_unwired, 1);
    assert_eq!(app.protocol_drops.cache_saturated, 0);
}

// `Started` spawns the encode, so this needs a runtime.
#[tokio::test]
async fn request_protocol_counts_a_saturated_protocol_cache() {
    let mut app = app_with_rooms(Vec::new());
    let (tx, _rx) = tokio::sync::mpsc::channel(MEDIA_WORKERS * 2);
    app.set_media_sender(tx);

    let key = MediaKey::new(Uuid::nil(), "mxc://example.com/a".to_owned());
    app.image_cache
        .insert(key.clone(), ImageState::Ready(ready_image()));

    // Every slot mid-encode, so `evict_lru_where` can free nothing.
    for i in 0..PROTOCOL_CACHE_LIMIT {
        let filler = ProtocolKey {
            media: MediaKey::new(Uuid::nil(), format!("mxc://example.com/filler{i}")),
            size: Size::new(8, 4),
        };
        app.proto_cache
            .insert(filler.clone(), ProtocolState::Encoding);
        touch_lru(&mut app.proto_cache_order, &filler);
    }

    assert_eq!(
        app.request_protocol(key.clone(), Size::new(8, 4)),
        ProtocolRequest::CacheSaturated
    );
    assert_eq!(app.protocol_drops.cache_saturated, 1);

    // Self-healing: once a slot settles, the same request goes through.
    let settled = ProtocolKey {
        media: MediaKey::new(Uuid::nil(), "mxc://example.com/filler0".to_owned()),
        size: Size::new(8, 4),
    };
    app.proto_cache
        .insert(settled, ProtocolState::Failed("boom".to_owned()));
    assert_eq!(
        app.request_protocol(key, Size::new(8, 4)),
        ProtocolRequest::Started
    );
    assert_eq!(app.protocol_drops.cache_saturated, 1);
}

/// #49: the Sixel retransmit state is per-preview, not global.
///
/// The counter selects between two encodings of the same image that differ
/// only in a trailing SGR, so a preview inheriting odd parity opens on the
/// alternate variant; the deadline used to be a main-loop local, so an
/// interval that elapsed while nothing was open left the next preview due
/// for a retransmit on its very first tick.
#[test]
fn opening_a_media_preview_resets_the_sixel_retransmit_state() {
    let mut app = app_with_rooms(Vec::new());
    app.sixel_preview_generation = 7;
    app.sixel_preview_refresh_after = Instant::now() - Duration::from_secs(60);

    app.open_popup(PopupKind::MediaPreview);

    assert_eq!(app.sixel_preview_generation, 0);
    assert!(app.sixel_preview_refresh_after > Instant::now());
}

/// Only the media preview owns this state; other popups leave it alone.
#[test]
fn opening_another_popup_leaves_the_sixel_retransmit_state_alone() {
    let mut app = app_with_rooms(Vec::new());
    app.sixel_preview_generation = 7;
    let deadline = Instant::now() - Duration::from_secs(60);
    app.sixel_preview_refresh_after = deadline;

    app.open_popup(PopupKind::Help);

    assert_eq!(app.sixel_preview_generation, 7);
    assert_eq!(app.sixel_preview_refresh_after, deadline);
}
