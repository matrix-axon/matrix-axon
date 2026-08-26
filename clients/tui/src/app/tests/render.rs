//! Rendered message geometry: HTML body formatting, the layout cache that
//! decides when a message must be measured again, and the frames that cache
//! draws.

use super::support::*;
use crate::app::*;
use crate::ui::draw;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use unicode_width::UnicodeWidthStr;

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
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
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
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
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
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::from([(key.clone(), 2)]),
        &RelationContext::default(),
        MessageDensity::Dense,
        TimeFormat::H24,
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
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
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
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::from([(key.clone(), 2)]),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
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
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::from([(key.clone(), 2)]),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
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

// ---------------------------------------------------------------------------
// The draw-time selection overlay (#235)
// ---------------------------------------------------------------------------

/// The fact the whole design rests on: the two markers occupy the same columns,
/// so the marker's contribution to `body_prefix_cols` — and therefore the wrap
/// width and `MessageLayout.ranges` — does not depend on the selection.
///
/// Widen one of them and every wrapped line of the selected message would move
/// while the cached ranges still described the old geometry, which is a scroll
/// desync rather than a cosmetic bug. Hence a test rather than a comment.
#[test]
fn selection_markers_are_the_same_width() {
    assert_eq!(
        UnicodeWidthStr::width(SELECTED_MARKER),
        UnicodeWidthStr::width(UNSELECTED_MARKER)
    );
}

/// The point of the change: moving the selection must not recompute anything.
///
/// Before this, `layout_recomputes` advanced once per `move_selected_message`
/// call, so holding a nav key re-parsed and re-wrapped the whole timeline on
/// every keystroke.
#[test]
fn moving_the_selection_is_a_layout_cache_hit() {
    let mut app = app_with_timeline(vec![
        text_message("$one:example.com", "first"),
        text_message("$two:example.com", "second"),
        text_message("$three:example.com", "third"),
    ]);
    app.messages.selection = Some("$one:example.com".to_owned());
    app.ensure_message_layout();
    let recomputes = app.messages.layout_recomputes;

    for id in ["$two:example.com", "$three:example.com", "$one:example.com"] {
        app.messages.selection = Some(id.to_owned());
        app.ensure_message_layout();
    }

    assert_eq!(
        app.messages.layout_recomputes, recomputes,
        "selection changes styling, not geometry, so they must all be cache hits"
    );
    assert_eq!(
        app.messages.layout_checks,
        recomputes + 3,
        "and the checks must still be counted"
    );
}

/// The overlay's own output, on the header line and nowhere else.
///
/// Moved here from `app/tests/rooms.rs` when the styling moved out of
/// `message_layout`; it asserted the same three things about the baked-in
/// version. The scope is deliberate and unchanged: a selected message that
/// wraps gets the background bar on its header row only, not on its
/// continuation rows.
#[test]
fn the_selection_overlay_highlights_only_the_header_line() {
    let colors = TuiConfig::test_default().colors;
    let event = text_message("$message:example.com", "hello");
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
    );
    let mut page = layout.lines.clone();

    overlay_selection_on_page(
        &mut page,
        0,
        &layout.ranges,
        &[&event],
        Some("$message:example.com"),
        &colors,
        80,
        true,
    );

    assert_eq!(page[1].style.bg, Some(colors.selection_background));
    assert_eq!(page[1].width(), 80);
    assert_eq!(page[2].style.bg, None);
    // Untouched by the overlay, so still what the layout built.
    assert_eq!(layout.lines[1].style.bg, None);
}

/// `highlight_selected_line = false` — the default — still marks the selected
/// message and recolours its timestamp, it just does not paint the line. That
/// split is what `selected_line_style` encodes, and it is easy to lose when the
/// three effects move from one call site to another.
#[test]
fn the_selection_overlay_marks_the_header_without_the_line_style() {
    let colors = TuiConfig::test_default().colors;
    let event = text_message("$message:example.com", "hello");
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
    );
    let mut page = layout.lines.clone();

    overlay_selection_on_page(
        &mut page,
        0,
        &layout.ranges,
        &[&event],
        Some("$message:example.com"),
        &colors,
        80,
        false,
    );

    assert_eq!(page[1].spans[0].content, SELECTED_MARKER);
    assert_eq!(page[1].spans[0].style.fg, Some(colors.selected_room));
    assert_eq!(page[1].spans[1].style.fg, Some(colors.selected_room));
    assert_eq!(
        page[1].style.bg, None,
        "no bar without the highlight option"
    );
    // The marker replaced the neutral one in place, so the line is still the
    // width the layout measured.
    assert_eq!(page[1].width(), layout.lines[1].width());
}

/// The overlay indexes into a *page*, not the whole layout, so it has to map
/// the layout line through `page_start`. Off by one and it paints the wrong
/// message; miss the bounds check and a selection scrolled off the top wraps
/// around to some unrelated row.
#[test]
fn the_selection_overlay_is_confined_to_the_page_it_is_given() {
    let colors = TuiConfig::test_default().colors;
    let events: Vec<EventDto> = (0..4)
        .map(|i| text_message(&format!("${i}:example.com"), "hello"))
        .collect();
    let refs: Vec<&EventDto> = events.iter().collect();
    let sender_labels = vec!["@alice:example.com".to_owned(); events.len()];
    let layout = message_layout(
        refs.as_slice(),
        sender_labels.as_slice(),
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
    );
    // Message 2's header, in whole-layout coordinates.
    let header = layout.ranges[2].start;

    // A page that starts at message 2's header: the overlay lands on its
    // first row.
    let mut page = layout.lines[header..].to_vec();
    overlay_selection_on_page(
        &mut page,
        header,
        &layout.ranges,
        refs.as_slice(),
        Some("$2:example.com"),
        &colors,
        80,
        true,
    );
    assert_eq!(page[0].style.bg, Some(colors.selection_background));
    assert!(
        page[1..].iter().all(|line| line.style.bg.is_none()),
        "exactly one line is restyled"
    );

    // The same selection with a page that starts *after* it: nothing to
    // restyle, and in particular not row zero.
    let start = header + 1;
    let mut page = layout.lines[start..].to_vec();
    overlay_selection_on_page(
        &mut page,
        start,
        &layout.ranges,
        refs.as_slice(),
        Some("$2:example.com"),
        &colors,
        80,
        true,
    );
    assert!(
        page.iter().all(|line| line.style.bg.is_none()),
        "a selection above the page must not be painted onto it"
    );
    assert!(
        page.iter().all(|line| line
            .spans
            .first()
            .is_none_or(|span| span.content != SELECTED_MARKER)),
        "and must not move the marker either"
    );
}

/// A selection naming an event that is not in the timeline — a stale id left
/// over from a room switch, or a message that scrolled out of the loaded slice
/// — must be a no-op rather than restyling whatever happens to be at line
/// zero.
#[test]
fn an_unknown_selection_restyles_nothing() {
    let colors = TuiConfig::test_default().colors;
    let event = text_message("$message:example.com", "hello");
    let sender_labels = vec!["@alice:example.com".to_owned()];
    let layout = message_layout(
        &[&event],
        sender_labels.as_slice(),
        &colors,
        80,
        &HashMap::new(),
        &HashMap::new(),
        &ImageThumbRows::new(),
        &RelationContext::default(),
        MessageDensity::Normal,
        TimeFormat::H24,
    );
    let mut page = layout.lines.clone();

    overlay_selection_on_page(
        &mut page,
        0,
        &layout.ranges,
        &[&event],
        Some("$gone:example.com"),
        &colors,
        80,
        true,
    );

    assert_eq!(page, layout.lines);
}

// ---------------------------------------------------------------------------
// Render equality: a cached layout must draw what a fresh one would (#245)
// ---------------------------------------------------------------------------
//
// Everything above asserts on the digest — whether `layout_recomputes`
// advanced. That answers "did the cache decide to recompute?", not "does the
// cached layout draw what a fresh one would?", and the second question is the
// one users experience. The two come apart in both directions: a layout stored
// under the wrong key, a stale `layout_image_thumb_rows` read by `draw`, or
// `layout.ranges` and `layout.lines` describing different text all keep the
// digest honest and the screen wrong — and the last of those is the silent
// scroll desync `layout_cache.rs` opens by warning about.
//
// `layout_cache.rs` also asks for its per-event hash to match `render.rs`'s
// `event.*` reads exactly, and nothing but a hand-run grep enforced that. The
// fixtures below mutate one input class each and then compare a cached frame
// against an uncached one, which enforces it automatically for every class they
// cover.

/// Wide enough for the three-column layout, and tall enough that these
/// fixtures fit without scrolling — so a row a mutation adds or drops moves
/// visible content instead of falling off the bottom of the pane.
const FRAME_WIDTH: u16 = 100;
const FRAME_HEIGHT: u16 = 40;

const IMAGE_MXC: &str = "mxc://example.com/photo";

fn timeline_room() -> RoomDto {
    room("!room:example.com", Some("#room:example.com"), Some("Room"))
}

fn app_with_timeline(events: Vec<EventDto>) -> App {
    let room = timeline_room();
    let mut app = app_with_rooms(vec![room.clone()]);
    app.rooms.selected = Some(0);
    app.messages.events.insert(RoomKey::from(&room), events);
    app
}

/// The seeded timeline, for the in-place mutations below.
///
/// An edit, a redaction and a membership verb change all keep the event id, so
/// nothing but the digest's own projections stands between the cache and a
/// stale frame — which is exactly what makes them worth rendering twice.
fn timeline_events(app: &mut App) -> &mut Vec<EventDto> {
    app.messages
        .events
        .get_mut(&RoomKey::from(&timeline_room()))
        .expect("the seeded timeline")
}

fn text_message(event_id: &str, body: &str) -> EventDto {
    event_with_id(
        event_id,
        "m.room.message",
        Some(body),
        serde_json::json!({ "msgtype": "m.text", "body": body }),
    )
}

fn html_message(event_id: &str, body: &str, html: &str) -> EventDto {
    let mut event = text_message(event_id, body);
    event.content = Some(serde_json::json!({
        "msgtype": "m.text",
        "body": body,
        "format": "org.matrix.custom.html",
        "formatted_body": html,
    }));
    event
}

fn image_message(event_id: &str) -> EventDto {
    event_with_id(
        event_id,
        "m.room.message",
        Some("photo.jpg"),
        serde_json::json!({
            "msgtype": "m.image",
            "body": "photo.jpg",
            "filename": "photo.jpg",
            "url": IMAGE_MXC,
        }),
    )
}

fn thread_reply(event_id: &str, root_event_id: &str, body: &str) -> EventDto {
    let mut event = text_message(event_id, body);
    event.relates_to = Some(serde_json::json!({
        "rel_type": "m.thread",
        "event_id": root_event_id,
    }));
    event
}

fn reply_to(event_id: &str, target_event_id: &str, body: &str) -> EventDto {
    let mut event = text_message(event_id, body);
    event.relates_to = Some(serde_json::json!({
        "m.in_reply_to": { "event_id": target_event_id }
    }));
    event
}

fn membership_event(event_id: &str, membership: &str) -> EventDto {
    event_with_state_key(
        event_id,
        "m.room.member",
        Some("@bob:example.com"),
        None,
        serde_json::json!({ "membership": membership }),
    )
}

/// A 40x40 px image: two rows at the 10x20 halfblocks font size, against the
/// six `IMAGE_THUMB_ROWS` an undecoded image reserves.
fn decoded_thumbnail() -> ImageState {
    ImageState::Ready(Arc::new(image::DynamicImage::new_rgb8(40, 40)))
}

fn draw_frame(app: &mut App) -> Buffer {
    let mut terminal =
        Terminal::new(TestBackend::new(FRAME_WIDTH, FRAME_HEIGHT)).expect("terminal");
    let completed = terminal
        .draw(|frame| draw(frame, app))
        .expect("draw succeeds");
    completed.buffer.clone()
}

/// Fill the layout cache from the current state, and settle the state `draw`
/// itself owns.
///
/// Both halves matter. Priming is what gives the comparison its teeth: the
/// cached pass afterwards holds a layout built *before* the mutation, which is
/// precisely the stale frame a digest that missed an input would draw.
/// Settling matters because `draw` mutates `App` in ways that have nothing to
/// do with the layout — `clear_media_preview`, `force_terminal_clear`, the
/// pin-to-bottom scroll sentinel — and a first-frame-only effect would
/// otherwise register as a difference between the two frames compared below.
fn prime_layout_cache(app: &mut App) {
    let _ = draw_frame(app);
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
        .collect()
}

/// Draw `app` twice — once through the layout cache, once with the cache
/// defeated — and require the two frames to be identical.
///
/// Cells are compared whole, styles included, so a selection highlight or a
/// trust glyph that only the fresh pass applies fails here even when the text
/// is unchanged.
#[track_caller]
fn assert_cached_frame_matches_uncached(app: &mut App, what: &str) {
    let cached = draw_frame(app);

    // Clear the key but keep the layout: `draw` unwraps
    // `cached_message_layout()`, so dropping the layout as well would put that
    // unwrap under test instead of the frame it produces.
    app.messages.layout_key = None;
    let uncached = draw_frame(app);

    let difference = cached
        .content
        .iter()
        .zip(uncached.content.iter())
        .enumerate()
        .find(|(_, (cached_cell, uncached_cell))| cached_cell != uncached_cell);
    if let Some((index, (cached_cell, uncached_cell))) = difference {
        let x = index as u16 % cached.area.width;
        let y = index as u16 / cached.area.width;
        panic!(
            "{what}: the cached layout drew a different frame than an uncached one.\n\
             first difference at column {x}, row {y}\n\
             \x20 cached cell: {cached_cell:?}\n\
             uncached cell: {uncached_cell:?}\n\
             \x20 cached row: {:?}\n\
             uncached row: {:?}",
            row_text(&cached, y),
            row_text(&uncached, y),
        );
    }
    assert_eq!(cached.area, uncached.area, "{what}: frame areas differ");
}

/// The plain form of the property, over every input class at once: the cache
/// is a substitute for recomputing, so a frame drawn from it must be
/// indistinguishable from one drawn fresh.
///
/// No mutation here — this is the baseline the class-by-class tests below
/// build on, and it is the one that would catch a layout filed under the wrong
/// key or a `layout_image_thumb_rows` snapshot that disagrees with the layout
/// it was stored beside.
#[test]
fn a_cached_frame_matches_an_uncached_one_across_every_input_class() {
    let mut app = app_with_timeline(vec![
        html_message(
            "$html:example.com",
            "bold link",
            "<strong>bold</strong> <a href=\"https://example.com\">link</a>",
        ),
        image_message("$image:example.com"),
        text_message("$root:example.com", "thread root"),
        thread_reply("$member:example.com", "$root:example.com", "in the thread"),
        reply_to(
            "$reply:example.com",
            "$html:example.com",
            "answering the first one",
        ),
        membership_event("$join:example.com", "join"),
        {
            let mut event = text_message("$redacted:example.com", "this body was removed");
            event.redacted = true;
            event
        },
        message_with_reactions(
            "$reacted:example.com",
            vec![("\u{1f44d}", tally(2, false, &[]))],
        ),
        text_message("$last:example.com", "the last line"),
    ]);
    // Two inputs the events above do not carry: the decoded thumbnail lives in
    // `image_cache`, the server reply count in `thread_summaries`.
    app.image_cache.insert(
        MediaKey::new(Uuid::nil(), IMAGE_MXC.to_owned()),
        decoded_thumbnail(),
    );
    app.thread_summaries.insert(
        RoomKey::from(&timeline_room()),
        HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 3,
            },
        )]),
    );
    app.messages.selection = Some("$last:example.com".to_owned());

    prime_layout_cache(&mut app);
    assert_cached_frame_matches_uncached(&mut app, "a timeline holding every input class");
}

/// An edit keeps the event id, so only the body projections change — and
/// geometry comes from the *HTML* wrap, not the plaintext fallback.
///
/// `body` is deliberately left alone here so that `display_body()` is
/// unchanged and only `formatted_body()` differs. Mutating both would let this
/// pass on `display_body()` alone and never exercise the `formatted_body()`
/// line in the digest at all, which is the whole reason the digest reads two
/// projections of the same field rather than one.
#[test]
fn an_edited_html_body_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        html_message("$html:example.com", "before", "<em>before</em>"),
        text_message("$after:example.com", "below the edit"),
    ]);
    prime_layout_cache(&mut app);

    // Long enough to wrap onto a second row, so a stale frame differs in
    // geometry and not only in the text on one line.
    let events = timeline_events(&mut app);
    events[0].content = Some(serde_json::json!({
        "msgtype": "m.text",
        "body": "before",
        "format": "org.matrix.custom.html",
        "formatted_body": "<em>before</em>, and then a good deal more markup than \
                           the first pass carried, enough of it to wrap onto a \
                           second rendered row",
    }));

    assert_cached_frame_matches_uncached(&mut app, "an edited HTML body");
}

/// The one input class no event field announces: `image_thumb_rows` is derived
/// from `image_cache`, so a decode landing changes the reserved row count with
/// every event byte-identical.
#[test]
fn a_landed_thumbnail_decode_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        image_message("$image:example.com"),
        text_message("$after:example.com", "below the image"),
    ]);
    // Primed while undecoded, so the image reserves the default
    // `IMAGE_THUMB_ROWS`.
    prime_layout_cache(&mut app);

    app.image_cache.insert(
        MediaKey::new(Uuid::nil(), IMAGE_MXC.to_owned()),
        decoded_thumbnail(),
    );

    assert_cached_frame_matches_uncached(&mut app, "a thumbnail decode landing");
}

/// Reacting from inside the TUI patches the target's aggregate in place, which
/// adds a tally row under it.
#[test]
fn a_local_reaction_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        text_message("$one:example.com", "worth a thumbs up"),
        text_message("$after:example.com", "below the reaction"),
    ]);
    prime_layout_cache(&mut app);

    app.apply_local_reaction(
        "$one:example.com",
        "\u{1f44d}",
        "$react:example.com".to_owned(),
    );

    assert_cached_frame_matches_uncached(&mut app, "a locally applied reaction");
}

/// A reply whose target is older than the loaded slice renders an unresolved
/// context line until the background fetch lands it in `reply_targets`. The
/// replying event never changes, so this rides entirely on
/// `RelationContext::replies` being in the digest.
#[test]
fn a_late_reply_target_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        reply_to("$reply:example.com", "$absent:example.com", "answering"),
        text_message("$after:example.com", "below the reply"),
    ]);
    prime_layout_cache(&mut app);

    app.reply_targets.insert(
        (Uuid::nil(), "$absent:example.com".to_owned()),
        text_message("$absent:example.com", "the message being answered"),
    );

    assert_cached_frame_matches_uncached(&mut app, "a reply target arriving late");
}

/// A server thread summary lands on a root with no members in the slice, so a
/// badge appears with every event byte-identical — the `thread_badges` half of
/// the same `RelationContext` coverage.
#[test]
fn a_server_thread_summary_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        text_message("$root:example.com", "thread root"),
        text_message("$after:example.com", "below the root"),
    ]);
    prime_layout_cache(&mut app);

    app.thread_summaries.insert(
        RoomKey::from(&timeline_room()),
        HashMap::from([(
            "$root:example.com".to_owned(),
            crate::api::ThreadSummaryDto {
                root_event_id: "$root:example.com".to_owned(),
                reply_count: 3,
            },
        )]),
    );

    assert_cached_frame_matches_uncached(&mut app, "a server thread summary arriving");
}

/// A live thread member promoted into the main timeline shows context pointing
/// back at its root, and that root is resolved from `reply_targets` when it is
/// older than the slice. This is the third and last `RelationContext` map —
/// `thread_contexts`, which no other test here reaches — and the member event
/// itself never changes, so the frame rides entirely on that map being hashed.
#[test]
fn a_late_thread_root_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        thread_reply(
            "$member:example.com",
            "$absent:example.com",
            "in the thread",
        ),
        text_message("$after:example.com", "below the thread member"),
    ]);
    // Without the promotion the member is filtered out of the main timeline
    // altogether, and `thread_contexts` is never consulted.
    app.promoted_thread_events
        .insert("$member:example.com".to_owned());
    prime_layout_cache(&mut app);

    app.reply_targets.insert(
        (Uuid::nil(), "$absent:example.com".to_owned()),
        text_message("$absent:example.com", "the root of the thread"),
    );

    assert_cached_frame_matches_uncached(&mut app, "a thread root arriving late");
}

/// `membership_change()` derives from `content`, which is a `serde_json::Value`
/// the digest cannot hash directly — it goes through the projection instead.
/// This checks the projection is the one `render.rs` reads.
#[test]
fn a_membership_verb_change_draws_the_same_frame_cached_and_uncached() {
    let mut app = app_with_timeline(vec![
        membership_event("$member:example.com", "join"),
        text_message("$after:example.com", "below the membership line"),
    ]);
    prime_layout_cache(&mut app);

    timeline_events(&mut app)[0].content = Some(serde_json::json!({ "membership": "ban" }));

    assert_cached_frame_matches_uncached(&mut app, "a membership verb changing");
}

/// A redaction replaces a wrapped body with `[redacted]`, dropping a row.
#[test]
fn a_redaction_draws_the_same_frame_cached_and_uncached() {
    let long = "a body long enough to wrap across more than one rendered row, so that \
                replacing it with the redaction placeholder changes the geometry and not \
                merely the text on the line";
    let mut app = app_with_timeline(vec![
        text_message("$one:example.com", long),
        text_message("$after:example.com", "below the redaction"),
    ]);
    prime_layout_cache(&mut app);

    let events = timeline_events(&mut app);
    events[0].redacted = true;
    events[0].redaction_event_id = Some("$redact:example.com".to_owned());

    assert_cached_frame_matches_uncached(&mut app, "a message being redacted");
}

/// Selection is a layout input today, and #235 proposes taking it out of the
/// digest on the grounds that `"> "` and `"  "` are both two columns, so the
/// wrap is selection-independent. This is the guard for that: whether the
/// highlight is baked into the layout or applied at draw time, the cached and
/// uncached frames have to agree on where it lands.
///
/// Run under both `highlight_selected_line` settings, because they are
/// separate render paths — with it off the marker moves and the line style
/// does not.
#[test]
fn moving_the_selection_draws_the_same_frame_cached_and_uncached() {
    for highlight in [false, true] {
        // Long enough to wrap, so the selection spans more than one rendered
        // row: a highlight keyed on anything but the layout's own ranges would
        // cover the first line only.
        let wrapped = "a selected message long enough to wrap across two rendered rows, so \
                       the highlight has to follow the whole range and not just its first line";
        let mut app = app_with_timeline(vec![
            text_message("$one:example.com", wrapped),
            text_message("$two:example.com", wrapped),
        ]);
        app.display.highlight_selected_line = highlight;
        app.messages.selection = Some("$one:example.com".to_owned());
        prime_layout_cache(&mut app);

        app.messages.selection = Some("$two:example.com".to_owned());

        assert_cached_frame_matches_uncached(
            &mut app,
            &format!("the selection moving with highlight_selected_line = {highlight}"),
        );
    }
}
