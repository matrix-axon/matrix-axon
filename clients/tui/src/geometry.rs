//! Pure cell geometry for the TUI.
//!
//! Layout arithmetic the renderer needs but that touches no `App` state and
//! draws nothing: where a modal sits, how tall the compose box is, which row a
//! thumbnail starts on, how far the room list scrolls. Keeping it here lets it
//! be tested on plain numbers rather than through a `TestBackend` frame.
//!
//! Not to be confused with `app::layout_cache`, which memoizes rendered
//! *message* layout — that is a cache keyed on content, not geometry.

use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use std::ops::Range;

pub(crate) fn image_thumbnail_spec(
    messages_area: Rect,
    message_width: usize,
    message_range: &Range<usize>,
    scroll: usize,
    page_size: usize,
    thumb_h: usize,
    body_rows: usize,
) -> Option<(Rect, Size)> {
    let visible_end = scroll.saturating_add(page_size);
    let thumb_start = message_range.start.saturating_add(body_rows);
    let thumb_end = thumb_start.saturating_add(thumb_h);
    if thumb_start < scroll || thumb_end > visible_end {
        return None;
    }
    let body_w = (message_width as u16).saturating_sub(2);
    if body_w < 4 {
        return None;
    }
    let size = Size::new(body_w, thumb_h as u16);
    let rect = Rect::new(
        messages_area.x.saturating_add(3),
        messages_area
            .y
            .saturating_add(1)
            .saturating_add((thumb_start - scroll) as u16),
        body_w,
        thumb_h as u16,
    );
    Some((rect, size))
}

pub(crate) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

pub(crate) fn centered_size(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Visual layout of the compose buffer for a given inner width: the total number
/// of rows it occupies and the cursor's `(row, col)` within them. Hard line
/// breaks (`\n`, from Shift+Enter) start a new row; long logical lines wrap by
/// character count, matching the input `Paragraph`'s wrapping. A two-column
/// prompt/continuation prefix is accounted for on every logical line.
pub(crate) fn compose_layout(
    buffer: &str,
    cursor: usize,
    inner_width: usize,
) -> (usize, usize, usize) {
    const PREFIX: usize = 2;
    let iw = inner_width.max(1);
    let mut total_rows = 0usize;
    let mut cur_row = 0usize;
    let mut cur_col = PREFIX;
    let mut offset = 0usize; // byte offset of the current logical line's start
    for segment in buffer.split('\n') {
        let text_cols = PREFIX + segment.chars().count();
        let height = text_cols.div_ceil(iw).max(1);
        let seg_start = offset;
        let seg_end = offset + segment.len();
        if cursor >= seg_start && cursor <= seg_end {
            let col = PREFIX + buffer[seg_start..cursor].chars().count();
            let (sub_row, sub_col) = if col > iw {
                (1 + (col - iw) / iw, (col - iw) % iw)
            } else {
                (0, col)
            };
            cur_row = total_rows + sub_row;
            cur_col = sub_col;
        }
        total_rows += height;
        offset = seg_end + 1; // skip the '\n'
    }
    (total_rows.max(1), cur_row, cur_col)
}

pub(crate) fn divider_aware_room_scroll(
    current_scroll: usize,
    selected_vis: usize,
    page_size: usize,
    visible_len: usize,
    pinned_visible_count: usize,
) -> usize {
    let divider_before = |scroll: usize| {
        pinned_visible_count > 0
            && pinned_visible_count < visible_len
            && selected_vis >= pinned_visible_count
            && pinned_visible_count > scroll
    };

    let mut scroll = current_scroll;
    if selected_vis < scroll {
        scroll = selected_vis;
    } else if page_size > 0 {
        let selected_row = selected_vis - scroll + usize::from(divider_before(scroll));
        if selected_row >= page_size {
            scroll = (scroll + selected_row + 1 - page_size).min(selected_vis);
            if !divider_before(scroll) && scroll > 0 {
                let candidate = scroll - 1;
                let candidate_row =
                    selected_vis - candidate + usize::from(divider_before(candidate));
                if candidate_row < page_size {
                    scroll = candidate;
                }
            }
        }
    }
    let divider_rows = usize::from(pinned_visible_count > 0 && pinned_visible_count < visible_len);
    let max_scroll = visible_len
        .saturating_add(divider_rows)
        .saturating_sub(page_size)
        .min(visible_len.saturating_sub(1));
    scroll.min(max_scroll)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_layout_single_line() {
        // "> hello" with the cursor after it: one row, cursor at col 2 + 5.
        let (rows, row, col) = compose_layout("hello", 5, 80);
        assert_eq!((rows, row, col), (1, 0, 7));
    }

    #[test]
    fn compose_layout_counts_hard_line_breaks() {
        // Two hard lines; cursor at end of the second ("hi\nthere", cursor=8).
        let (rows, row, col) = compose_layout("hi\nthere", 8, 80);
        assert_eq!(rows, 2);
        assert_eq!(row, 1); // second visual row
        assert_eq!(col, 2 + 5); // prefix + "there"
    }

    #[test]
    fn compose_layout_cursor_at_start_of_second_line() {
        // Cursor right after the '\n' (offset 3 in "hi\nx") sits at the start of
        // the second row, at the continuation-prefix column.
        let (rows, row, col) = compose_layout("hi\nx", 3, 80);
        assert_eq!((rows, row, col), (2, 1, 2));
    }

    #[test]
    fn compose_layout_wraps_long_line() {
        // A logical line longer than the width wraps onto extra rows.
        let buffer = "a".repeat(10);
        let (rows, row, col) = compose_layout(&buffer, 10, 5);
        // prefix(2) + 10 chars = 12 cols over width 5 -> 3 rows.
        assert_eq!(rows, 3);
        // cursor at col 12: overflow 7 -> row 1 + 7/5 = 2, col 7%5 = 2.
        assert_eq!((row, col), (2, 2));
    }

    #[test]
    fn divider_aware_room_scroll_counts_separator_against_page() {
        // Five rooms plus a pinned/unpinned separator need six rendered rows.
        // With a five-row viewport and the last room selected, scroll by one
        // room index so the selected room is not clipped off the bottom.
        assert_eq!(divider_aware_room_scroll(0, 4, 5, 5, 2), 1);
    }

    #[test]
    fn divider_aware_room_scroll_does_not_scroll_past_selected_room() {
        // A one-row viewport cannot show the divider and the first unpinned room
        // at once. Scroll to the selected room itself; the render loop skips the
        // divider when the viewport starts at the boundary.
        assert_eq!(divider_aware_room_scroll(0, 2, 1, 5, 2), 2);
    }

    #[test]
    fn divider_aware_room_scroll_drops_separator_after_it_scrolls_out() {
        // Once the divider is above the final viewport, it should not consume a
        // row from the selected-room fit calculation.
        assert_eq!(divider_aware_room_scroll(0, 19, 5, 20, 2), 15);
    }

    #[test]
    fn divider_aware_room_scroll_keeps_tail_selected_when_divider_is_adjacent() {
        // If backing up to fill the bottom row would bring the divider back into
        // view and clip the selected room, keep the selected room visible.
        assert_eq!(divider_aware_room_scroll(0, 19, 5, 20, 16), 16);
    }

    #[test]
    fn thumbnail_geometry_requires_the_full_reserved_region() {
        let area = Rect::new(10, 5, 50, 12);
        let range = 2..9;

        let (rect, size) =
            image_thumbnail_spec(area, 48, &range, 0, 10, 6, 1).expect("fully visible thumbnail");

        assert_eq!(rect, Rect::new(13, 9, 46, 6));
        assert_eq!(size, Size::new(46, 6));
        assert!(image_thumbnail_spec(area, 48, &range, 4, 10, 6, 1).is_none());
        assert!(image_thumbnail_spec(area, 48, &range, 0, 8, 6, 1).is_none());
    }

    #[test]
    fn thumbnail_geometry_is_independent_of_sender_label_width() {
        let area = Rect::new(0, 0, 30, 10);
        let range = 0..7;

        let (rect, _) =
            image_thumbnail_spec(area, 28, &range, 0, 8, 6, 1).expect("visible thumbnail");

        assert_eq!(rect.x, 3);
        assert_eq!(rect.width, 26);
    }
}
