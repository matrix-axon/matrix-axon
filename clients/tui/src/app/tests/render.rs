//! Rendered message geometry: HTML body formatting, and the layout cache that
//! decides when a message must be measured again.

use super::support::*;
use crate::app::*;

#[test]
fn formatted_body_renders_supported_html_styles() {
    let colors = TuiConfig::test_default().colors;
    let event = EventDto {
        content: Some(serde_json::json!({
            "msgtype": "m.text",
            "body": "bold link code",
            "format": "org.matrix.custom.html",
            "formatted_body": "<strong>bold</strong> <a href=\"https://example.com\">link</a> <code>code</code>"
        })),
        body: Some("bold link code".to_owned()),
        ..event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("bold link code"),
            serde_json::json!({ "msgtype": "m.text", "body": "bold link code" }),
        )
    };
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let lines = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
        false,
    )
    .lines;

    assert!(lines[1].spans.iter().any(|span| {
        span.content.contains("bold") && span.style.add_modifier.contains(Modifier::BOLD)
    }));
    assert!(lines[1].spans.iter().any(|span| {
        span.content.contains("link")
            && span.style.fg == Some(colors.status)
            && span.style.add_modifier.contains(Modifier::UNDERLINED)
    }));
    assert!(lines[1]
        .spans
        .iter()
        .any(|span| { span.content.contains("code") && span.style.fg == Some(colors.input_hint) }));
}

#[test]
fn formatted_body_strips_unsupported_html_and_falls_back_when_empty() {
    let colors = TuiConfig::test_default().colors;
    let event = EventDto {
        content: Some(serde_json::json!({
            "msgtype": "m.text",
            "body": "fallback",
            "format": "org.matrix.custom.html",
            "formatted_body": "<script>alert('x')</script>"
        })),
        body: Some("fallback".to_owned()),
        ..event_with_id(
            "$message:example.com",
            "m.room.message",
            Some("fallback"),
            serde_json::json!({ "msgtype": "m.text", "body": "fallback" }),
        )
    };
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let lines = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
        false,
    )
    .lines;

    let text = lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("fallback"));
    assert!(!text.contains("alert"));
}

#[test]
fn image_layout_counts_caption_and_cached_thumbnail_rows_once() {
    let colors = TuiConfig::test_default().colors;
    let event = event_with_id(
        "$image:example.com",
        "m.room.message",
        Some("caption"),
        serde_json::json!({
            "msgtype": "m.image",
            "body": "caption",
            "filename": "photo.jpg",
            "url": "mxc://example.com/photo"
        }),
    );
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let key = (event.account_id, "mxc://example.com/photo".to_owned());
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::from([(key.clone(), 2)]),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
        false,
    );

    assert_eq!(layout.image_body_rows.get(&key), Some(&2));
    assert_eq!(layout.ranges, vec![1..5]);
    assert_eq!(layout.lines.len(), 5);
}

#[test]
fn normal_layout_puts_body_below_sender_header() {
    let colors = TuiConfig::test_default().colors;
    let event = event_with_id(
        "$message:example.com",
        "m.room.message",
        Some("hello"),
        serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
    );
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
        false,
    );

    // Header row carries the sender but no body; the body is a separate row.
    let header: String = layout.lines[1]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(header.contains("@alice:example.com"));
    assert!(!header.contains("hello"));

    let body: String = layout.lines[2]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    // Body indents to align under the sender (marker "  " + "HH:MM:SS ").
    assert_eq!(body, format!("{}hello", " ".repeat(11)));
    assert_eq!(layout.ranges, vec![1..3]);
}

#[test]
fn normal_layout_image_body_rows_includes_header_row() {
    let colors = TuiConfig::test_default().colors;
    let event = event_with_id(
        "$image:example.com",
        "m.room.message",
        Some("caption"),
        serde_json::json!({
            "msgtype": "m.image",
            "body": "caption",
            "filename": "photo.jpg",
            "url": "mxc://example.com/photo"
        }),
    );
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let key = (event.account_id, "mxc://example.com/photo".to_owned());
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::from([(key.clone(), 2)]),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
        false,
    );

    // Same 2 caption rows + 2 thumbnail rows as the dense case, but the
    // thumbnail offset now includes the separate sender header line.
    assert_eq!(layout.image_body_rows.get(&key), Some(&3));
    assert_eq!(layout.ranges, vec![1..6]);
    assert_eq!(layout.lines.len(), 6);
}

#[test]
fn image_reply_offsets_thumbnail_below_the_reply_line() {
    let colors = TuiConfig::test_default().colors;
    let mut event = event_with_id(
        "$image:example.com",
        "m.room.message",
        Some("caption"),
        serde_json::json!({
            "msgtype": "m.image",
            "body": "caption",
            "filename": "photo.jpg",
            "url": "mxc://example.com/photo"
        }),
    );
    // Mark the image as a reply: a reply-context line renders between the
    // header and the body, so the thumbnail must drop below it.
    event.relates_to = Some(serde_json::json!({
        "m.in_reply_to": { "event_id": "$parent:example.com" }
    }));
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let key = (event.account_id, "mxc://example.com/photo".to_owned());
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        None,
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::from([(key.clone(), 2)]),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
        false,
    );

    // header(1) + reply line(1) + caption(2) = 4 rows above the thumbnail.
    // Before the fix this was 3, so the thumbnail overwrote the filename.
    assert_eq!(layout.image_body_rows.get(&key), Some(&4));
    // 4 rows + 2 thumbnail rows = 6 message rows after the date separator.
    assert_eq!(layout.ranges, vec![1..7]);
    assert_eq!(layout.lines.len(), 7);
}

/// Two `ensure_message_layout` calls with nothing changed in between must
/// not recompute. Proven by corrupting the stored ranges and checking the
/// corruption survives: a recompute would overwrite it.
#[test]
fn unchanged_inputs_do_not_recompute_the_layout() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$one:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        )],
    );

    app.ensure_message_layout();
    let key = app.messages.layout_key;
    assert!(key.is_some(), "the first call computes a layout");

    // Corrupt the cached layout itself, not a copy of it: if the second
    // call recomputes, the corruption is overwritten.
    if let Some(layout) = app.messages.layout.as_mut() {
        layout.ranges = vec![99..100, 100..101];
    }
    app.ensure_message_layout();

    assert_eq!(app.messages.layout_key, key, "the digest is stable");
    assert_eq!(
        app.cached_message_ranges(),
        [99..100, 100..101],
        "a cache hit must not recompute"
    );
}

/// The overlay counters must distinguish a hit from a recompute — that is
/// their entire purpose, since a cache that never hits looks identical on
/// screen and only costs more.
#[test]
fn the_layout_counters_separate_hits_from_recomputes() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$one:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        )],
    );

    app.ensure_message_layout();
    assert_eq!(
        (app.messages.layout_checks, app.messages.layout_recomputes),
        (1, 1)
    );

    // Three hits: checked each time, recomputed none of them.
    for _ in 0..3 {
        app.ensure_message_layout();
    }
    assert_eq!(
        (app.messages.layout_checks, app.messages.layout_recomputes),
        (4, 1)
    );

    // A real change recomputes once more.
    app.messages.width = 20;
    app.ensure_message_layout();
    assert_eq!(
        (app.messages.layout_checks, app.messages.layout_recomputes),
        (5, 2)
    );
}

/// A trust verdict changing must invalidate the layout.
///
/// `sender_trust` renders as a glyph that reserves two columns, so it feeds
/// `body_prefix_cols` and therefore the wrap width. Omitting it from the
/// digest left the safety glyph stale *and* the ranges wrapped against the
/// wrong width — a security-relevant badge disagreeing with what nav and
/// scrolling measure against (#229 review).
#[test]
fn a_sender_trust_change_recomputes_the_layout() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    app.messages.events.insert(
        key.clone(),
        vec![event_with_id(
            "$one:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        )],
    );

    app.ensure_message_layout();
    let before = app.messages.layout_key;

    if let Some(events) = app.messages.events.get_mut(&key) {
        events[0].sender_trust = Some("unverified".to_owned());
    }
    app.ensure_message_layout();

    assert_ne!(
        app.messages.layout_key, before,
        "the trust glyph is rendered and shifts the wrap width, so it is a layout input"
    );
}

/// A rendered change that lives in `content` must invalidate the layout.
///
/// `content` is a `serde_json::Value` and cannot be hashed directly, so the
/// digest goes through the renderer's own accessors. This asserts the
/// outcome, not which accessor delivers it: `display_body` and
/// `membership_change` both project this change, so it stays covered if
/// either is present. The digest lists both because it mirrors `render.rs`
/// exactly — over-hashing costs a spurious re-layout, under-hashing renders
/// lines that disagree with the ranges nav measures.
#[test]
fn a_membership_change_recomputes_the_layout() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.display.show_state_events = true;
    let key = RoomKey::from(&room);
    app.messages.events.insert(
        key.clone(),
        vec![event_with_id(
            "$join:example.com",
            "m.room.member",
            None,
            serde_json::json!({ "membership": "join" }),
        )],
    );

    app.ensure_message_layout();
    let before = app.messages.layout_key;

    if let Some(events) = app.messages.events.get_mut(&key) {
        events[0].content = Some(serde_json::json!({ "membership": "leave" }));
    }
    app.ensure_message_layout();

    assert_ne!(
        app.messages.layout_key, before,
        "a membership verb change is rendered, so it is a layout input"
    );
}

/// The pane getting narrower re-wraps, so it must invalidate. Width is a
/// layout input that the old event-id-keyed cache did not cover.
#[test]
fn a_width_change_recomputes_the_layout() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$one:example.com",
            "m.room.message",
            Some("a rather long message that will wrap differently when narrowed"),
            serde_json::json!({
                "msgtype": "m.text",
                "body": "a rather long message that will wrap differently when narrowed"
            }),
        )],
    );

    app.messages.width = 80;
    app.ensure_message_layout();
    let wide = app.messages.layout_key;

    app.messages.width = 20;
    app.ensure_message_layout();

    assert_ne!(app.messages.layout_key, wide, "width must invalidate");
}

/// An edit replaces a message body in place, keeping its event id. The
/// previous cache keyed on event ids alone, so this was a false hit: the
/// ranges kept describing the pre-edit text while `draw` rendered the new
/// text, which is how a scroll desync starts.
#[test]
fn an_edited_body_recomputes_the_layout() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    let key = RoomKey::from(&room);
    app.messages.events.insert(
        key.clone(),
        vec![event_with_id(
            "$one:example.com",
            "m.room.message",
            Some("short"),
            serde_json::json!({ "msgtype": "m.text", "body": "short" }),
        )],
    );

    app.ensure_message_layout();
    let before = app.messages.layout_key;

    if let Some(events) = app.messages.events.get_mut(&key) {
        events[0].body = Some("a much longer body after the edit".to_owned());
    }
    app.ensure_message_layout();

    assert_ne!(
        app.messages.layout_key, before,
        "an edit keeps the event id, so the digest must cover the body"
    );
}

/// Reacting from inside the TUI patches the target message's aggregate in
/// place (`apply_local_reaction`), so the digest must notice. Remote
/// reactions are a different path — they arrive as raw `m.reaction` rows
/// that never touch the target's aggregate — and are not covered here.
#[test]
fn a_local_reaction_recomputes_the_layout() {
    let room = room("!room:example.com", Some("#room:example.com"), Some("Room"));
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(
        RoomKey::from(&room),
        vec![event_with_id(
            "$one:example.com",
            "m.room.message",
            Some("hello"),
            serde_json::json!({ "msgtype": "m.text", "body": "hello" }),
        )],
    );

    app.ensure_message_layout();
    let before = app.messages.layout_key;

    app.apply_local_reaction(
        "$one:example.com",
        "\u{1f44d}",
        "$react:example.com".to_owned(),
    );
    app.ensure_message_layout();

    assert_ne!(
        app.messages.layout_key, before,
        "a reaction the client applied itself must invalidate the layout"
    );
}
