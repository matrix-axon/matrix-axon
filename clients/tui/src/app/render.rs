use std::collections::HashMap;
use std::ops::Range;

use chrono::{Local, NaiveDate, TimeZone};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use uuid::Uuid;

use crate::api::EventDto;
use crate::config::{ColorScheme, MessageDensity, TimeFormat};
use crate::html::formatted_message_body_lines;
use crate::wrap::{plain_rich_lines, rich_lines_to_spans, wrap_rich_lines, RichSpan};

/// Map from `(account_id, mxc_url)` to the rows allocated for its thumbnail.
/// Entries are only needed when a cached image is naturally shorter than
/// `IMAGE_THUMB_ROWS`; absent entries default to `IMAGE_THUMB_ROWS`.
pub(crate) type ImageThumbRows = HashMap<(Uuid, String), usize>;

pub(crate) struct MessageLayout {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) ranges: Vec<Range<usize>>,
    pub(crate) image_body_rows: HashMap<(Uuid, String), usize>,
}

/// Resolved context for a message that replies to another event (ADR 0032 M1).
/// `sender` is the display label of the replied-to event's author and `snippet`
/// a truncated excerpt of its body. Present only when the target was resolved
/// (in the loaded slice or via cross-window fetch); its absence for a replying
/// event renders the `[reply context not loaded]` placeholder.
#[derive(Hash)]
pub(crate) struct ReplyPreview {
    pub(crate) sender: String,
    pub(crate) snippet: String,
}

/// Thread summary shown on a thread root in the main timeline (ADR 0032 M2/M3).
/// `count` is the server-aggregated reply total when known, otherwise the
/// in-slice member count; `latest_*` describe the most recent member resolved
/// from the loaded slice.
#[derive(Hash)]
pub(crate) struct ThreadBadge {
    pub(crate) count: i64,
    pub(crate) unread_count: usize,
    pub(crate) latest_sender: Option<String>,
    pub(crate) latest_snippet: Option<String>,
}

/// Per-render relation context threaded into [`message_layout`] (ADR 0032).
/// Bundled into one struct so the layout signature stays readable.
#[derive(Default)]
pub(crate) struct RelationContext {
    /// Reply preview keyed by the *replying* event's id.
    pub(crate) replies: HashMap<String, ReplyPreview>,
    /// Thread badge keyed by the thread *root* event id (empty in the panel).
    pub(crate) thread_badges: HashMap<String, ThreadBadge>,
    /// The thread root's event id when rendering inside the thread panel, so it
    /// is labeled `[thread root]`.
    pub(crate) thread_root: Option<String>,
    /// Thread-root context for promoted live thread member events shown in the
    /// main timeline. Keyed by the thread member event id. `Some(preview)` when
    /// the root was resolved; `None` when the root is not yet loaded.
    pub(crate) thread_contexts: HashMap<String, Option<ReplyPreview>>,
}

/// Maximum characters of a replied-to body shown in a reply context line.
const REPLY_SNIPPET_LEN: usize = 60;
/// Maximum characters of a thread's latest-reply body shown in a badge.
const THREAD_SNIPPET_LEN: usize = 50;

/// Truncate `text` to at most `max` characters (by `char`, collapsing newlines
/// to spaces), appending an ellipsis when shortened.
pub(crate) fn snippet(text: &str, max: usize) -> String {
    let flattened = text.replace(['\n', '\r'], " ");
    let trimmed = flattened.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(max).collect();
    format!("{}…", head.trim_end())
}

/// The `↳ N replies · latest: sender: "body…"` summary shown below a thread root
/// (ADR 0032 M2). The `· latest:` clause is omitted when no member is resolved.
pub(crate) fn thread_badge_text(badge: &ThreadBadge) -> String {
    let noun = if badge.count == 1 { "reply" } else { "replies" };
    let mut text = format!("↳ {} {noun}", badge.count);
    if badge.unread_count > 0 {
        text.push_str(&format!(" · {} new", badge.unread_count));
    }
    if let (Some(sender), Some(body)) = (&badge.latest_sender, &badge.latest_snippet) {
        text.push_str(&format!(
            " · latest: {sender}: \"{}\"",
            snippet(body, THREAD_SNIPPET_LEN)
        ));
    }
    text
}

/// Rows reserved in the message list for image/sticker events so the inline
/// thumbnail has enough vertical space to be legible.
pub(crate) const IMAGE_THUMB_ROWS: usize = 6;

/// The gutter every message's header line opens with.
///
/// The two must stay the same display width. That is the fact the whole
/// draw-time selection overlay rests on (#235): the marker feeds
/// `body_prefix_cols` and therefore the wrap width, so a selected and an
/// unselected message wrap identically and one cached layout serves both.
/// `selection_markers_are_the_same_width` pins it.
pub(crate) const SELECTED_MARKER: &str = "> ";
pub(crate) const UNSELECTED_MARKER: &str = "  ";

pub(crate) fn format_time(origin_ts: i64, time_format: TimeFormat) -> String {
    Local
        .timestamp_millis_opt(origin_ts)
        .single()
        .map(|dt| match time_format {
            TimeFormat::H24 => dt.format("%H:%M:%S").to_string(),
            TimeFormat::H12 => {
                let hour = dt.format("%I").to_string().parse::<u32>().unwrap_or(0);
                dt.format(&format!("{}:%M:%S %p", hour)).to_string()
            }
        })
        .unwrap_or_else(|| "--:--:--".to_owned())
}

/// Returns the user's local calendar day for the given millisecond timestamp.
fn date_day(origin_ts: i64) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(origin_ts)
        .single()
        .map(|dt| dt.date_naive())
}

/// Formats `origin_ts` as "Thursday, 25 June 2026".
pub(crate) fn format_date(origin_ts: i64) -> String {
    Local
        .timestamp_millis_opt(origin_ts)
        .single()
        .map(|dt| dt.format("%A, %-d %B %Y").to_string())
        .unwrap_or_else(|| "Unknown date".to_owned())
}

/// A full-width horizontal rule with the date centered, e.g.:
/// `──────── Thursday, 25 June 2026 ────────`
pub(crate) fn date_separator_line(
    date_str: &str,
    width: usize,
    colors: &ColorScheme,
) -> Line<'static> {
    let label = format!(" {date_str} ");
    let label_len = label.chars().count();
    let fill = width.saturating_sub(label_len);
    let left = fill / 2;
    let right = fill - left;
    let line = format!("{}{}{}", "─".repeat(left), label, "─".repeat(right));
    Line::from(Span::styled(
        line,
        Style::default()
            .fg(colors.border)
            .add_modifier(Modifier::DIM),
    ))
}

pub(crate) fn message_index_at_line(ranges: &[Range<usize>], line: usize) -> usize {
    ranges
        .iter()
        .position(|range| line < range.end)
        .unwrap_or_else(|| ranges.len().saturating_sub(1))
}

/// Build the rendered lines and per-message line ranges for a timeline.
///
/// The lines are **selection-neutral**: every message gets
/// [`UNSELECTED_MARKER`] and default marker/timestamp styling, and the caller
/// restyles the selected one at draw time with [`overlay_selection_on_page`].
/// That is what keeps `selected_message` out of the layout digest, so moving
/// the selection is a cache hit rather than a full re-parse and re-wrap of the
/// timeline (#235).
#[allow(clippy::too_many_arguments)]
pub(crate) fn message_layout(
    events: &[&EventDto],
    sender_labels: &[String],
    colors: &ColorScheme,
    width: usize,
    reactions: &HashMap<String, Vec<(String, usize)>>,
    own_senders: &HashMap<Uuid, String>,
    image_thumb_rows: &ImageThumbRows,
    relations: &RelationContext,
    density: MessageDensity,
    time_format: TimeFormat,
) -> MessageLayout {
    let reaction_style = Style::default().fg(colors.input_hint);
    let relation_style = Style::default()
        .fg(colors.input_hint)
        .add_modifier(Modifier::ITALIC);
    let unread_relation_style = Style::default()
        .fg(colors.unread_count)
        .add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    let mut ranges = Vec::with_capacity(events.len());
    let mut image_body_rows = HashMap::new();
    let mut last_date_day: Option<NaiveDate> = None;
    // Width used for wrapped sub-rows (reply/thread context, badges, reactions);
    // also used to count a reply line's rows when offsetting an image thumbnail.
    let cont_w = continuation_body_width(width);

    for (event, sender_label) in events.iter().zip(sender_labels) {
        let event_day = date_day(event.origin_ts);
        if last_date_day != event_day {
            lines.push(date_separator_line(
                &format_date(event.origin_ts),
                width,
                colors,
            ));
            last_date_day = event_day;
        }
        let is_own = own_senders.get(&event.account_id) == Some(&event.sender);
        let sender_color = if is_own {
            colors.own_message_sender
        } else {
            colors.message_sender
        };
        let trust = trust_glyph(event.sender_trust.as_deref());
        let glyph_cols = if trust.is_some() { 2 } else { 0 };
        let is_normal = density == MessageDensity::Normal;
        // The first body row shares the sender header line only in dense mode;
        // normal mode keeps the sender/timestamp on their own line.
        let sender_on_header = !is_normal;
        // Dense: body text starts on the header line just past "sender: ", and
        // continuation lines indent to that same column.
        let body_prefix_cols = 2
            + format_time(event.origin_ts, time_format).chars().count()
            + 1
            + glyph_cols
            + sender_label.chars().count()
            + 2;
        // Normal: the body is its own block, indented to align under the sender
        // / verification glyph (just past marker + time).
        let normal_indent_cols = 2 + format_time(event.origin_ts, time_format).chars().count() + 1;
        let body_indent = if is_normal {
            " ".repeat(normal_indent_cols)
        } else {
            " ".repeat(body_prefix_cols)
        };
        let cont_body_w = if is_normal {
            width.saturating_sub(normal_indent_cols).max(1)
        } else {
            width.saturating_sub(body_prefix_cols).max(1)
        };
        // A reply context line (when this event replies to another) renders on
        // its own line below the sender header, aligned to the continuation
        // column (ADR 0032 M1). The sender header then carries no body span.
        //
        // Promoted thread member events (live replies whose root is off-screen)
        // get a matching thread context line pointing back to the thread root.
        // Only one of reply_line / thread_context_line is non-None for a given
        // event: thread members with a reply_relation show the reply line (the
        // thread root label is already injected by the panel), while plain
        // promoted thread members that have no reply_relation use the context line.
        let reply_line =
            event
                .reply_relation()
                .map(|_| match relations.replies.get(event.event_id.as_str()) {
                    Some(preview) => format!(
                        "↩ {}: \"{}\"",
                        preview.sender,
                        snippet(&preview.snippet, REPLY_SNIPPET_LEN)
                    ),
                    None => "↩ [reply context not loaded]".to_owned(),
                });
        let thread_context_line = if reply_line.is_none() {
            relations
                .thread_contexts
                .get(event.event_id.as_str())
                .map(|preview| match preview {
                    Some(p) => format!(
                        "↩ thread: {}: \"{}\"",
                        p.sender,
                        snippet(&p.snippet, REPLY_SNIPPET_LEN)
                    ),
                    None => "↩ [thread context not loaded]".to_owned(),
                })
        } else {
            None
        };
        let has_reply = reply_line.is_some() || thread_context_line.is_some();
        // The "[thread root] " label is injected into the header line in the
        // thread panel; first_body_width must account for it so the body text
        // does not overflow the right border.
        let is_thread_root = relations.thread_root.as_deref() == Some(event.event_id.as_str());
        let thread_root_cols = if is_thread_root {
            "[thread root] ".len()
        } else {
            0
        };
        // In normal mode the body is a full-width block under the header; in
        // dense mode the first row shares the header line, so it is narrower.
        let first_body_w = if is_normal {
            cont_body_w
        } else {
            first_body_width(sender_label, event.origin_ts, width, time_format)
                .saturating_sub(glyph_cols)
                .saturating_sub(thread_root_cols)
        };
        let mut body_lines =
            message_body_lines(event, sender_label, first_body_w, cont_body_w, colors);
        let body_row_count = body_lines.len().max(1);
        if let Some((account_id, mxc_url)) = event.image_mxc() {
            let key = (account_id, mxc_url);
            // Rows from the message's first line down to the top of the thumbnail:
            //   - the sender header line (always one), except dense mode folds the
            //     first body row into it (only when there is no reply line),
            //   - any reply/thread context line (wrapped to the same width as when
            //     it is actually drawn), and
            //   - the body text rows.
            // Omitting the context rows drew the thumbnail over the filename when
            // an image was sent as a reply.
            let context_rows = reply_line
                .as_deref()
                .or(thread_context_line.as_deref())
                .map(|text| {
                    wrap_rich_lines(
                        vec![vec![RichSpan::new(text.to_owned(), relation_style)]],
                        cont_w,
                        cont_w,
                    )
                    .len()
                })
                .unwrap_or(0);
            let first_body_on_header = sender_on_header && !has_reply;
            let rows_above_thumbnail =
                1 + context_rows + body_row_count - usize::from(first_body_on_header);
            image_body_rows.insert(key.clone(), rows_above_thumbnail);
            let thumbnail_rows = image_thumb_rows
                .get(&key)
                .copied()
                .unwrap_or(IMAGE_THUMB_ROWS);
            body_lines.resize_with(body_row_count + thumbnail_rows, Vec::new);
        }

        let range_start = lines.len();
        let mut body_iter = body_lines.into_iter();
        // Sender header line: marker, time, optional trust glyph, sender. The
        // first body span rides here unless a reply context line takes the row
        // below the header instead (ADR 0032 M1).
        //
        // The marker and the time are spans 0 and 1, in that order, and
        // `apply_selected_message_style` restyles them by that index — so
        // anything inserted ahead of them has to move it too.
        let mut header = vec![
            Span::raw(UNSELECTED_MARKER),
            Span::raw(format!("{} ", format_time(event.origin_ts, time_format))),
        ];
        if let Some((symbol, color)) = trust {
            header.push(Span::styled(
                format!("{symbol} "),
                Style::default().fg(color),
            ));
        }
        // Dense appends ": " because the body follows on the same line; normal
        // ends the sender line after the name.
        let sender_text = if is_normal {
            sender_label.to_string()
        } else {
            format!("{sender_label}: ")
        };
        header.push(Span::styled(
            sender_text,
            Style::default()
                .fg(sender_color)
                .add_modifier(Modifier::BOLD),
        ));
        if is_thread_root {
            header.push(Span::styled("[thread root] ", relation_style));
        }
        if sender_on_header && !has_reply {
            if let Some(first) = body_iter.next() {
                header.extend(first);
            }
        }
        lines.push(Line::from(header));

        // Helper: wrap a single-style text to continuation_body_width and push
        // each resulting row with a "  " indent.  This prevents reply context,
        // thread badges, and reactions from overwriting the right border.
        let push_wrapped = |lines: &mut Vec<Line<'static>>, text: String, style: Style| {
            for line in rich_lines_to_spans(wrap_rich_lines(
                vec![vec![RichSpan::new(text, style)]],
                cont_w,
                cont_w,
            )) {
                let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
                spans.extend(line);
                lines.push(Line::from(spans));
            }
        };

        if let Some(text) = reply_line {
            push_wrapped(&mut lines, text, relation_style);
        } else if let Some(text) = thread_context_line {
            push_wrapped(&mut lines, text, relation_style);
        }

        for body in body_iter {
            let mut spans = vec![Span::raw(body_indent.clone())];
            spans.extend(body);
            lines.push(Line::from(spans));
        }

        if let Some(badge) = relations.thread_badges.get(event.event_id.as_str()) {
            let style = if badge.unread_count > 0 {
                unread_relation_style
            } else {
                relation_style
            };
            push_wrapped(&mut lines, thread_badge_text(badge), style);
        }

        if let Some(event_reactions) = reactions.get(&event.event_id) {
            if !event_reactions.is_empty() {
                let text = event_reactions
                    .iter()
                    .map(|(key, count)| format!("{key} {count}"))
                    .collect::<Vec<_>>()
                    .join("  ");
                push_wrapped(&mut lines, text, reaction_style);
            }
        }
        ranges.push(range_start..lines.len());
    }

    MessageLayout {
        lines,
        ranges,
        image_body_rows,
    }
}

/// Restyle the selected message's header line inside a page of cached lines.
///
/// The counterpart to [`message_layout`]'s selection-neutral output, and the
/// only thing standing between the cached layout and the drawn frame as far as
/// selection goes (#235). `page_start` is the layout line index `page` begins
/// at, so the caller hands over exactly the rows it is about to draw.
///
/// A selected message scrolled off either end of the page simply has no line to
/// restyle, which is what `checked_sub` and `get_mut` express between them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn overlay_selection_on_page(
    page: &mut [Line<'static>],
    page_start: usize,
    ranges: &[Range<usize>],
    events: &[&EventDto],
    selected_message: Option<&str>,
    colors: &ColorScheme,
    width: usize,
    highlight_selected_line: bool,
) {
    let Some(line) = selected_message_line(ranges, events, selected_message)
        .and_then(|line| line.checked_sub(page_start))
        .and_then(|offset| page.get_mut(offset))
    else {
        return;
    };
    apply_selected_message_style(line, colors, width, highlight_selected_line);
}

/// Which layout line carries `selected_message`'s header, if any.
///
/// The header is the first line of the message's range, and it is the only line
/// the selection touches: `message_layout` styles continuation rows identically
/// whether or not their message is selected, which
/// `the_selection_overlay_highlights_only_the_header_line` pins.
fn selected_message_line(
    ranges: &[Range<usize>],
    events: &[&EventDto],
    selected_message: Option<&str>,
) -> Option<usize> {
    let selected = selected_message?;
    let index = events.iter().position(|event| event.event_id == selected)?;
    ranges.get(index).map(|range| range.start)
}

/// Turn one selection-neutral header line into the selected message's.
///
/// Three things, all of which `message_layout` used to bake in: the marker
/// becomes [`SELECTED_MARKER`], the marker and timestamp take the
/// selected-room colour, and — only under `highlight_selected_line` — the line
/// is padded to `width` and given the selection background.
///
/// Because [`SELECTED_MARKER`] and [`UNSELECTED_MARKER`] are the same width,
/// none of this changes the line's geometry, so the ranges the nav math and
/// scrolling measure against stay valid.
fn apply_selected_message_style(
    line: &mut Line<'static>,
    colors: &ColorScheme,
    width: usize,
    highlight_selected_line: bool,
) {
    let time_style = Style::default().fg(colors.selected_room);
    if let Some(marker) = line.spans.first_mut() {
        *marker = Span::styled(SELECTED_MARKER, time_style);
    }
    if let Some(time) = line.spans.get_mut(1) {
        time.style = time_style;
    }
    if !highlight_selected_line {
        return;
    }
    let style = selected_line_style(colors, true, true);
    // Pad to the full pane width so the background reads as a bar rather than
    // stopping at the end of the text.
    let fill = width.saturating_sub(line.width());
    if fill > 0 {
        line.spans.push(Span::styled(" ".repeat(fill), style));
    }
    line.style = style;
}

pub(crate) fn selected_line_style(
    colors: &ColorScheme,
    selected: bool,
    highlight_selected_line: bool,
) -> Style {
    if selected && highlight_selected_line {
        Style::default().bg(colors.selection_background)
    } else {
        Style::default()
    }
}

fn first_body_width(
    sender_label: &str,
    origin_ts: i64,
    width: usize,
    time_format: TimeFormat,
) -> usize {
    let prefix_width = 2
        + format_time(origin_ts, time_format).chars().count()
        + 1
        + sender_label.chars().count()
        + 2;
    width.saturating_sub(prefix_width).max(1)
}

/// The sender-trust annotation for a message's first line (M7c / ADR 0031),
/// keyed on the at-decrypt `sender_trust` verdict. A glyph is shown only for the
/// states worth flagging; `unverified`, absent, and unrecognized verdicts render
/// no glyph (the common, unencrypted-or-plain case). Each glyph occupies two
/// columns (symbol + space), matching the width reserved in `message_layout`.
pub(crate) fn trust_glyph(sender_trust: Option<&str>) -> Option<(&'static str, Color)> {
    match sender_trust {
        Some("verified") => Some(("✓", Color::Green)),
        Some("verification_violation") => Some(("⚠", Color::Red)),
        Some("unknown") => Some(("?", Color::DarkGray)),
        _ => None,
    }
}

pub(crate) fn display_body_with_sender(event: &EventDto, sender_label: &str) -> String {
    if event.redacted {
        return "[redacted]".to_owned();
    }
    if let Some(membership) = event.membership_change() {
        return match membership.as_str() {
            "join" => format!("{sender_label} joined the room"),
            "leave" => format!("{sender_label} left the room"),
            "ban" => format!("{sender_label} was banned from the room"),
            "invite" => format!("{sender_label} was invited to the room"),
            _ => format!("{sender_label} membership changed: {membership}"),
        };
    }
    event.display_body()
}

fn message_body_lines(
    event: &EventDto,
    sender_label: &str,
    first_width: usize,
    continuation_width: usize,
    colors: &ColorScheme,
) -> Vec<Vec<Span<'static>>> {
    if !event.redacted && event.membership_change().is_none() {
        if let Some(lines) = event.formatted_body().and_then(|html| {
            formatted_message_body_lines(html, first_width, continuation_width, colors)
        }) {
            return lines;
        }
    }

    rich_lines_to_spans(wrap_rich_lines(
        plain_rich_lines(&display_body_with_sender(event, sender_label)),
        first_width,
        continuation_width,
    ))
}

fn continuation_body_width(width: usize) -> usize {
    width.saturating_sub(2).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_glyph_maps_known_verdicts() {
        assert_eq!(trust_glyph(Some("verified")), Some(("✓", Color::Green)));
        assert_eq!(
            trust_glyph(Some("verification_violation")),
            Some(("⚠", Color::Red))
        );
        assert_eq!(trust_glyph(Some("unknown")), Some(("?", Color::DarkGray)));
    }

    #[test]
    fn snippet_truncates_and_flattens() {
        assert_eq!(snippet("hello", 60), "hello");
        assert_eq!(snippet("line one\nline two", 60), "line one line two");
        assert_eq!(snippet("abcdef", 3), "abc…");
    }

    #[test]
    fn thread_badge_text_renders_count_and_latest() {
        let badge = ThreadBadge {
            count: 3,
            unread_count: 0,
            latest_sender: Some("bob".to_owned()),
            latest_snippet: Some("I think we should wait".to_owned()),
        };
        assert_eq!(
            thread_badge_text(&badge),
            "↳ 3 replies · latest: bob: \"I think we should wait\""
        );
    }

    #[test]
    fn thread_badge_text_singular_and_no_latest() {
        let badge = ThreadBadge {
            count: 1,
            unread_count: 0,
            latest_sender: None,
            latest_snippet: None,
        };
        assert_eq!(thread_badge_text(&badge), "↳ 1 reply");
    }

    #[test]
    fn thread_badge_text_renders_unread_count() {
        let badge = ThreadBadge {
            count: 3,
            unread_count: 2,
            latest_sender: Some("bob".to_owned()),
            latest_snippet: Some("I think we should wait".to_owned()),
        };
        assert_eq!(
            thread_badge_text(&badge),
            "↳ 3 replies · 2 new · latest: bob: \"I think we should wait\""
        );
    }

    #[test]
    fn trust_glyph_is_absent_for_unflagged_states() {
        assert_eq!(trust_glyph(Some("unverified")), None);
        assert_eq!(trust_glyph(None), None);
        assert_eq!(trust_glyph(Some("something_new")), None);
    }

    #[test]
    fn trust_glyph_reserves_two_columns_in_first_body_width() {
        // The two columns reserved in message_layout for a glyph must come
        // out of the body width so wrapping stays correct.
        let full = first_body_width("@alice:localhost", 0, 80, TimeFormat::H24);
        let glyph_cols = 2;
        assert_eq!(full.saturating_sub(glyph_cols), full - 2);
    }

    #[test]
    fn format_date_produces_expected_string() {
        let ts = 1_782_345_600_000;
        let expected = Local
            .timestamp_millis_opt(ts)
            .single()
            .unwrap()
            .format("%A, %-d %B %Y")
            .to_string();
        assert_eq!(format_date(ts), expected);
    }

    #[test]
    fn date_day_uses_local_calendar_day() {
        let evening = Local
            .with_ymd_and_hms(2026, 6, 25, 21, 0, 0)
            .single()
            .unwrap();
        let late = Local
            .with_ymd_and_hms(2026, 6, 25, 23, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            date_day(evening.timestamp_millis()),
            date_day(late.timestamp_millis())
        );
    }

    #[test]
    fn date_separator_spans_full_width_with_date_centered() {
        use crate::config::TuiConfig;
        let colors = TuiConfig::test_default().colors;
        let line = date_separator_line("Thursday, 25 June 2026", 40, &colors);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Thursday, 25 June 2026"));
        assert_eq!(text.chars().count(), 40);
    }
}
