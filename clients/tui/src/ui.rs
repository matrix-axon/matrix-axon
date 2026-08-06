use std::collections::HashMap;
use std::ops::Range;

use ratatui::buffer::CellDiffOption;
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::{FontSize, Image, Resize};
use unicode_width::UnicodeWidthChar;

use crate::app::{
    account_localpart, date_separator_line, format_date, format_time, message_layout,
    selected_line_style, AccountSelection, App, ImageState, ImageThumbRows, MediaKey, Mode,
    PopupKind, ProtocolKey, ProtocolState, RoomFilter, RoomKey, SearchKind, UnreadThreadEntry,
    VerificationDirection, VerificationFlow, VerificationStage, IMAGE_THUMB_ROWS,
};
use ratatui_image::picker::ProtocolType;

use crate::api::RoomDto;
use crate::command::{HELP_COMMANDS, HELP_COMMAND_GROUPS};
use crate::config::Shortcuts;
use crate::search::{
    SearchContextKey, SearchFormField, SearchGrouping, SearchResultsState, SearchScope,
};
use crate::wrap::{plain_rich_lines, rich_lines_to_spans, wrap_rich_lines};

/// Percentage of the screen (both axes) used for the media-preview popup.
/// Kept as a single constant so `preview_target_size` (which determines the
/// encoded image size) and `render_media_preview` (which draws the border)
/// always agree — a mismatch would produce a popup whose border doesn't match
/// the image it encloses.
const PREVIEW_MAX_PCT: u16 = 88;

pub(crate) fn draw(frame: &mut Frame<'_>, app: &mut App) {
    // On the frame immediately after the media-preview popup is closed, emit a
    // targeted Clear over the region the popup occupied.  Sixel/iTerm2 pixels
    // are not part of the ratatui cell model, so the cell-diff renderer cannot
    // erase them; without this explicit clear a ghost image lingers until
    // something else overwrites those cells.  Halfblocks are ordinary Unicode
    // characters and are already handled by the normal diff pass.
    if std::mem::take(&mut app.clear_media_preview)
        && !matches!(app.picker.protocol_type(), ProtocolType::Halfblocks)
    {
        let ghost_area = centered_rect(PREVIEW_MAX_PCT, PREVIEW_MAX_PCT, frame.area());
        frame.render_widget(Clear, ghost_area);
    }

    // Screen rects where a pixel-protocol image widget is drawn this frame.
    // Compared against `app.prev_image_rects` at the end of `draw()` to erase
    // ghost pixels left wherever an image was drawn last frame but not this one.
    let mut frame_image_rects: Vec<Rect> = Vec::new();

    let effective_input_lines = if let Some(max_lines) = app.display.max_input_lines {
        let inner_width = frame.area().width.saturating_sub(2) as usize;
        let actual_lines = if inner_width > 0 {
            let (rows, _, _) = compose_layout(&app.input.buffer, app.input.cursor, inner_width);
            rows.max(1) as u16
        } else {
            1
        };
        actual_lines.clamp(app.display.input_lines, max_lines)
    } else {
        app.display.input_lines
    };
    let input_box_height = effective_input_lines + 2; // content lines + top/bottom borders
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(input_box_height)])
        .split(frame.area());
    const ROOMS_NARROW_WIDTH: u16 = 32;
    const ROOMS_WIDE_MIN: u16 = 44;
    const ROOMS_WIDE_MAX: u16 = 70;
    const WIDE_THRESHOLD: u16 = 90;
    const ROOMS_WIDE_THRESHOLD: u16 = 110;
    const MIN_ROOMS_WIDTH: u16 = 15;

    let show_accounts = app.accounts_panel_visible();
    let show_rooms = app.rooms_panel_visible();
    let total_width = frame.area().width;
    let wide_enough = total_width >= WIDE_THRESHOLD;
    let rooms_wide = total_width >= ROOMS_WIDE_THRESHOLD;
    let accounts_width = app.display.accounts_panel_width;
    let base_rooms_width = if rooms_wide {
        (total_width / 3).clamp(ROOMS_WIDE_MIN, ROOMS_WIDE_MAX)
    } else {
        ROOMS_NARROW_WIDTH
    };
    let rooms_width = (base_rooms_width as i16 + app.display.rooms_panel_width_adj)
        .max(MIN_ROOMS_WIDTH as i16) as u16;

    let (accounts_area, rooms_area, messages_area) = match (show_accounts, show_rooms, wide_enough)
    {
        (true, true, true) => {
            // Three-column: [Accounts][Rooms][Messages]
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(accounts_width),
                    Constraint::Length(rooms_width),
                    Constraint::Min(20),
                ])
                .split(outer[0]);
            (Some(body[0]), Some(body[1]), body[2])
        }
        (true, true, false) => {
            // Narrow: [Accounts stacked on Rooms][Messages]
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(rooms_width), Constraint::Min(20)])
                .split(outer[0]);
            let total_acct_items = 1 + app.accounts.accounts.len();
            let acct_height = ((total_acct_items as u16 + 2).min(body[0].height / 3)).max(3);
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(acct_height), Constraint::Min(1)])
                .split(body[0]);
            (Some(left[0]), Some(left[1]), body[1])
        }
        (true, false, _) => {
            // Rooms hidden: [Accounts][Messages]
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(accounts_width), Constraint::Min(20)])
                .split(outer[0]);
            (Some(body[0]), None, body[1])
        }
        (false, true, _) => {
            // No accounts panel: [Rooms][Messages]
            let body = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(rooms_width), Constraint::Min(20)])
                .split(outer[0]);
            (None, Some(body[0]), body[1])
        }
        (false, false, _) => {
            // Messages only
            (None, None, outer[0])
        }
    };
    let command_response_will_popup =
        app.pending_command_response
            .as_deref()
            .is_some_and(|response| {
                let inner_width = outer[1].width.saturating_sub(2);
                let prefix_width = command_response_prefix_width(app);
                command_response_line_count(response, inner_width, prefix_width)
                    > usize::from(app.display.input_lines)
            });

    // Accounts panel
    if let Some(accounts_area) = accounts_area {
        let acct_page_size = accounts_area.height.saturating_sub(2).max(1) as usize;
        app.accounts.page_size = acct_page_size;

        let acct_search_query = match &app.mode {
            Mode::Search(SearchKind::Accounts, q) => Some(q.to_lowercase()),
            _ => None,
        };

        let all_acct_entries: Vec<(String, AccountSelection)> = std::iter::once((
            AccountSelection::All.display_label(None),
            AccountSelection::All,
        ))
        .chain(app.accounts.accounts.iter().enumerate().map(|(i, a)| {
            let selection = AccountSelection::Account(i);
            (selection.display_label(Some(&a.user_id)), selection)
        }))
        .filter(|(label, _)| {
            acct_search_query
                .as_ref()
                .is_none_or(|q| label.to_lowercase().contains(q.as_str()))
        })
        .collect();

        let acct_sel_pos = all_acct_entries
            .iter()
            .position(|(_, sel)| *sel == app.accounts.selected)
            .unwrap_or(0);
        let total_acct_items = all_acct_entries.len();

        if acct_sel_pos < app.accounts.scroll {
            app.accounts.scroll = acct_sel_pos;
        } else if acct_page_size > 0 && acct_sel_pos >= app.accounts.scroll + acct_page_size {
            app.accounts.scroll = acct_sel_pos + 1 - acct_page_size;
        }
        let acct_max_scroll = total_acct_items.saturating_sub(acct_page_size);
        app.accounts.scroll = app.accounts.scroll.min(acct_max_scroll);
        let acct_scroll = app.accounts.scroll;

        let acct_items: Vec<ListItem> = all_acct_entries
            .iter()
            .skip(acct_scroll)
            .take(acct_page_size)
            .map(|(label, sel)| {
                let is_sel = app.accounts.selected == *sel;
                let marker = if is_sel { ">" } else { " " };
                let style = if is_sel {
                    Style::default()
                        .fg(app.colors.selected_room)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(label.clone(), style),
                ]))
                .style(selected_line_style(
                    &app.colors,
                    is_sel,
                    app.display.highlight_selected_line,
                ))
            })
            .collect();

        let acct_active = app.mode == Mode::AccountList;
        let acct_border = if acct_active {
            Style::default()
                .fg(app.colors.selected_room)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.colors.border)
        };
        let acct_title = match &app.mode {
            Mode::Search(SearchKind::Accounts, q) => format!("Accounts  Search: {q}"),
            _ => "Accounts".to_owned(),
        };
        frame.render_widget(
            List::new(acct_items).block(
                Block::default()
                    .style(
                        Style::default()
                            .fg(app.colors.accounts_foreground)
                            .bg(app.colors.accounts_background),
                    )
                    .title(acct_title)
                    .borders(Borders::ALL)
                    .border_type(if acct_active {
                        BorderType::Double
                    } else {
                        BorderType::Plain
                    })
                    .border_style(acct_border),
            ),
            accounts_area,
        );
    }

    // Room list with account filter
    if let Some(rooms_area) = rooms_area {
        let visible_indices = app.visible_room_indices();
        let show_account_label =
            app.active_account_filter().is_none() && app.accounts_panel_visible();
        let rooms_selected_vis = app
            .rooms
            .selected
            .and_then(|sel| visible_indices.iter().position(|&i| i == sel))
            .unwrap_or(0);
        let rows_available = rooms_area.height.saturating_sub(2) as usize;
        let rooms_page_size = if rooms_wide {
            rows_available.max(1)
        } else {
            (rows_available / 2).max(1)
        };
        // Pinned rooms are sorted to the front, so the leading run of visible
        // rooms that are pinned marks the boundary for the separator (ADR 0038).
        let pinned_visible_count = visible_indices
            .iter()
            .take_while(|&&i| app.is_room_pinned(&RoomKey::from(&app.rooms.rooms[i])))
            .count();
        app.rooms.page_size = rooms_page_size;
        app.rooms.scroll = divider_aware_room_scroll(
            app.rooms.scroll,
            rooms_selected_vis,
            rooms_page_size,
            visible_indices.len(),
            pinned_visible_count,
        );
        let rooms_scroll = app.rooms.scroll;

        let separator_width = usize::from(rooms_area.width.saturating_sub(2)).max(1);
        let mut room_items: Vec<ListItem> = Vec::new();
        for (vis_pos, &full_index) in visible_indices.iter().enumerate().skip(rooms_scroll) {
            if room_items.len() >= rooms_page_size {
                break;
            }
            // Draw the pinned/unpinned divider only when this viewport shows the
            // first unpinned room with at least one pinned room above it.
            if vis_pos == pinned_visible_count && pinned_visible_count > 0 && vis_pos > rooms_scroll
            {
                room_items.push(ListItem::new(Line::from(Span::styled(
                    "─".repeat(separator_width),
                    Style::default()
                        .fg(app.colors.border)
                        .add_modifier(Modifier::DIM),
                ))));
            }
            if room_items.len() >= rooms_page_size {
                break;
            }
            let item = {
                let room = &app.rooms.rooms[full_index];
                let key = RoomKey::from(room);
                let unread_count = app.rooms.unread.get(&key).copied().unwrap_or_default();
                let is_selected = Some(full_index) == app.rooms.selected;
                let marker = if is_selected { ">" } else { " " };
                let unread_str = if unread_count > 0 {
                    format!(" ({unread_count})")
                } else {
                    String::new()
                };
                let latest = room
                    .last_event_id
                    .as_deref()
                    .map(|_| {
                        format!(
                            " {}",
                            format_time(room.last_activity_ts, app.display.time_format)
                        )
                    })
                    .unwrap_or_default();
                let alias = room
                    .canonical_alias
                    .as_deref()
                    .or(room.topic.as_deref())
                    .map(|value| format!(" {value}"))
                    .unwrap_or_default();
                let account_tag = if show_account_label {
                    room.account_user_id
                        .as_deref()
                        .map(|uid| {
                            let localpart = account_localpart(uid).unwrap_or(uid);
                            format!(" [{localpart}]")
                        })
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let title_style = if is_selected {
                    Style::default()
                        .fg(app.colors.selected_room)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                if rooms_wide {
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{marker}{} ", room_display_number(vis_pos))),
                        Span::styled(app.room_list_title(room), title_style),
                        Span::raw(account_tag),
                        Span::styled(unread_str, Style::default().fg(app.colors.unread_count)),
                        Span::raw(latest),
                        Span::raw(alias),
                    ]))
                } else {
                    ListItem::new(vec![
                        Line::from(vec![
                            Span::raw(format!("{marker}{} ", room_display_number(vis_pos))),
                            Span::styled(app.room_list_title(room), title_style),
                            Span::raw(account_tag),
                        ]),
                        Line::from(vec![
                            Span::raw("    "),
                            Span::styled(unread_str, Style::default().fg(app.colors.unread_count)),
                            Span::raw(format!("{latest}{alias}")),
                        ]),
                    ])
                }
                .style(selected_line_style(
                    &app.colors,
                    is_selected,
                    app.display.highlight_selected_line,
                ))
            };
            room_items.push(item);
        }
        let rooms_active = app.mode == Mode::RoomList;
        let rooms_border = if rooms_active {
            Style::default()
                .fg(app.colors.selected_room)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.colors.border)
        };
        let rooms_title = if let Mode::Search(SearchKind::Rooms, q) = &app.mode {
            format!("Rooms  Search: {q}")
        } else if let Mode::Search(SearchKind::RoomNameFilter, q) = &app.mode {
            format!("Rooms  Filter: {q}")
        } else {
            // Show the active sort, and the filter when it is not the default.
            match &app.room_filter {
                RoomFilter::All => format!("Rooms — {}", app.room_sort.label()),
                other => format!("Rooms — {} · {}", other.label(), app.room_sort.label()),
            }
        };
        let rooms = List::new(room_items).block(
            Block::default()
                .style(
                    Style::default()
                        .fg(app.colors.rooms_foreground)
                        .bg(app.colors.rooms_background),
                )
                .title(rooms_title.as_str())
                .borders(Borders::ALL)
                .border_type(if rooms_active {
                    BorderType::Double
                } else {
                    BorderType::Plain
                })
                .border_style(rooms_border),
        );
        frame.render_widget(rooms, rooms_area);
    }

    let message_page_size = usize::from(messages_area.height.saturating_sub(2)).max(1);
    let message_width = usize::from(messages_area.width.saturating_sub(2)).max(1);
    app.set_message_viewport(message_page_size, message_width);
    let selected_events = app.selected_events();
    let sender_labels = selected_events
        .iter()
        .map(|event| app.sender_label(event))
        .collect::<Vec<_>>();
    let reactions = app.selected_reactions();
    // Build thumbnail heights from the cache. The shared message layout adds
    // each image's wrapped label/caption rows and computes all line ranges once.
    let font_size = app.picker.font_size();
    let image_thumb_rows: ImageThumbRows = selected_events
        .iter()
        .filter_map(|event| {
            let (account_id, mxc_url) = event.image_mxc()?;
            let key = MediaKey::new(account_id, mxc_url.clone());
            let thumb_h = if let Some(ImageState::Ready(img)) = app.image_cache.get(&key) {
                let nat = Resize::natural_size(img, font_size);
                (nat.height as usize).clamp(1, IMAGE_THUMB_ROWS)
            } else {
                IMAGE_THUMB_ROWS
            };
            if thumb_h != IMAGE_THUMB_ROWS {
                Some(((account_id, mxc_url), thumb_h))
            } else {
                None
            }
        })
        .collect();
    let relations = app.relation_context(selected_events.as_slice());
    let layout = message_layout(
        selected_events.as_slice(),
        sender_labels.as_slice(),
        app.selected_message_id(),
        &app.colors,
        message_width,
        &reactions,
        &app.live.own_senders,
        &image_thumb_rows,
        &relations,
        app.display.message_density,
        app.display.time_format,
        app.display.highlight_selected_line,
    );
    let total_lines = layout
        .ranges
        .last()
        .map(|range| range.end)
        .unwrap_or_default();
    let max_scroll = total_lines.saturating_sub(message_page_size);
    let message_scroll = if app.messages.scroll == usize::MAX {
        max_scroll
    } else {
        app.messages.scroll.min(max_scroll)
    };
    let message_lines = layout
        .lines
        .iter()
        .skip(message_scroll)
        .take(message_page_size)
        .cloned()
        .collect::<Vec<_>>();
    let title = app
        .selected_room()
        .map(|room| app.room_list_title(room))
        .unwrap_or_else(|| "No room selected".to_owned());
    let messages_active = app.mode == Mode::MessageList;
    let messages_border = if messages_active {
        Style::default()
            .fg(app.colors.selected_room)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.colors.border)
    };
    let messages_title = if let Mode::Search(SearchKind::Messages, q) = &app.mode {
        format!("{title}  Search: {q}")
    } else if app.thread_panel.is_some() {
        format!("{title}  [in thread]")
    } else {
        title
    };
    let mut messages_block = Block::default()
        .style(
            Style::default()
                .fg(app.colors.messages_foreground)
                .bg(app.colors.messages_background),
        )
        .title(messages_title.as_str())
        .borders(Borders::ALL)
        .border_type(if messages_active {
            BorderType::Double
        } else {
            BorderType::Plain
        })
        .border_style(messages_border);
    // Live typing / read-receipt overlay for the open room (M18, ADR 0056),
    // rendered as a bottom border title so it never shifts the message layout.
    if let Some(status) = app.ephemeral_status_line() {
        messages_block = messages_block.title_bottom(Line::from(Span::styled(
            format!(" {status} "),
            Style::default().fg(app.colors.status),
        )));
    }
    let messages = Paragraph::new(message_lines).block(messages_block);
    frame.render_widget(messages, messages_area);

    // Media cards keep their caption on the normal message row and reserve six
    // rows immediately below it for the thumbnail. A graphic is rendered only
    // when that complete reserved region is visible; partial scrolling therefore
    // cannot paint outside the message's rows or cover adjacent text.
    let (media_requests, thumb_specs) = {
        let visible_end = message_scroll.saturating_add(message_page_size);
        let mut requests = Vec::new();
        let specs: Vec<(Rect, Size, MediaKey)> = selected_events
            .iter()
            .enumerate()
            .filter_map(|(idx, event)| {
                let (account_id, mxc_url) = event.image_mxc()?;
                let range = &layout.ranges[idx];
                if range.end <= message_scroll || range.start >= visible_end {
                    return None;
                }
                let media = MediaKey::new(account_id, mxc_url.clone());
                requests.push((media.clone(), event.image_is_encrypted()));
                let image_key = (account_id, mxc_url);
                let body_rows = layout.image_body_rows.get(&image_key).copied().unwrap_or(1);
                let thumb_h = image_thumb_rows
                    .get(&image_key)
                    .copied()
                    .unwrap_or(IMAGE_THUMB_ROWS);
                let (rect, size) = image_thumbnail_spec(
                    messages_area,
                    message_width,
                    range,
                    message_scroll,
                    message_page_size,
                    thumb_h,
                    body_rows,
                )?;
                Some((rect, size, media))
            })
            .collect();
        (requests, specs)
    };
    let layout_event_ids = selected_events
        .iter()
        .map(|event| event.event_id.clone())
        .collect();
    app.set_message_layout(layout_event_ids, layout.ranges);
    for (key, encrypted) in &media_requests {
        app.request_image(key.account_id, key.mxc_url.clone(), *encrypted);
    }
    // A background refresh (thumbnail re-render) must never paint over an open
    // modal popup: pixel-protocol graphics live outside ratatui's cell model, so
    // a Clear under the popup would not erase them.  Suppress any thumbnail that
    // overlaps the active modal's region.
    let blocking_popup = blocking_popup_area(app, frame.area());
    for (rect, size, media) in thumb_specs {
        app.request_protocol(media.clone(), size);
        let protocol_key = ProtocolKey { media, size };
        if let Some(ProtocolState::Ready(protocol)) = app.proto_cache.get(&protocol_key) {
            let inline = protocol.inline(app.sixel_inline_generation);
            // The widget anchors the image at `rect`'s top-left and paints it at
            // its own aspect-fit cell size — only this sub-rect holds pixels.
            // Collision/ghost tracking must use it; the full-width `rect` would
            // hide thumbnails whose real pixels sit clear of the popup.
            let drawn = image_draw_rect(rect, inline.size());
            if thumbnail_overlaps_blocking_popup(drawn, blocking_popup) {
                continue;
            }
            frame.render_widget(Image::new(inline), rect);
            frame_image_rects.push(drawn);
        }
    }
    // After all message-pane widgets have rendered, mark blank cells as
    // AlwaysUpdate when the view was rebuilt (thread panel toggled, or a room
    // switch via load_selected_timeline).  Blank cells (space + default colours)
    // are invisible to ratatui's diff when the background is Color::Reset, so
    // pixel-protocol ghost pixels from the previous view are never overwritten.
    // AlwaysUpdate forces ratatui to emit terminal codes for those cells,
    // clearing the stale pixels.
    //
    // We restrict this to cells whose symbol is a plain space: cells that
    // contain halfblock (▄▀) or text characters are already caught by the normal
    // diff (their symbol changed), and force-emitting ambiguous-width halfblock
    // chars without explicit MoveTo causes cursor drift that staggers the border.
    //
    // Crucially, confine the pass to the image rectangles (this frame's and the
    // previous frame's) rather than the whole message pane. Pixel ghosts can only
    // linger where an image was actually drawn; forcing every blank cell in the
    // pane made ratatui emit a single long write-run per row, and on rows that
    // carry an ambiguous-width glyph (a reaction emoji, a status badge) that run
    // crossed the glyph without an intervening MoveTo, drifting the cursor and
    // dropping the right border on that row until the next content change. Text
    // rows hold no out-of-band pixels, so the normal diff is enough for them.
    //
    // Skip the pass entirely under Halfblocks: that protocol draws images as
    // ordinary glyphs the normal diff already repaints (clear_image_ghosts skips
    // it for the same reason), so there are no out-of-band pixels to clear.
    if std::mem::take(&mut app.force_terminal_clear)
        && !matches!(app.picker.protocol_type(), ProtocolType::Halfblocks)
    {
        let regions: Vec<Rect> = app
            .prev_image_rects
            .iter()
            .chain(frame_image_rects.iter())
            .filter_map(|rect| {
                let clipped = rect.intersection(messages_area);
                (clipped.width > 0 && clipped.height > 0).then_some(clipped)
            })
            .collect();
        let buf = frame.buffer_mut();
        for region in regions {
            for y in region.top()..region.bottom() {
                for x in region.left()..region.right() {
                    if let Some(c) = buf.cell_mut((x, y)) {
                        // Only mark cells that have no diff option yet.  Sixel and
                        // Kitty image widgets set CellDiffOption::Skip on every
                        // cell they occupy; overwriting Skip with AlwaysUpdate
                        // causes ratatui to emit a space directly over the
                        // newly-rendered image, destroying it.  Cells that hold
                        // ghost pixels from a previous frame have diff_option ==
                        // None (the buffer-reset default), so they are safe to
                        // force-update.
                        if c.symbol() == " " && c.diff_option == CellDiffOption::None {
                            c.set_diff_option(CellDiffOption::AlwaysUpdate);
                        }
                    }
                }
            }
        }
    }

    // Pre-warm preview protocols for visible images so pressing 'v' shows the
    // image immediately rather than waiting for encoding to complete.  We cap
    // at `preview_warmup_count` (default 5) to bound proto_cache churn: warming
    // every visible image on every 100 ms tick fills the 32-entry cache with
    // Encoding entries and can starve thumbnail requests.
    let warmup_limit = app.display.preview_warmup_count;
    if warmup_limit > 0 {
        let preview_screen = frame.area();
        for (media, _) in media_requests.iter().take(warmup_limit) {
            if let Some(ImageState::Ready(img)) = app.image_cache.get(media) {
                if let Some(size) = preview_target_size(img, font_size, preview_screen) {
                    app.request_protocol(media.clone(), size);
                }
            }
        }
    }

    let (command_line, command_title, mut cursor_col) = match &app.mode {
        Mode::Search(kind, q) => {
            let kind_label = match kind {
                SearchKind::Rooms => "Rooms",
                SearchKind::Messages => "Messages",
                SearchKind::Accounts => "Accounts",
                SearchKind::RoomNameFilter => "Room filter",
            };
            let hint = match kind {
                SearchKind::RoomNameFilter => "  Enter: apply  Esc: cancel",
                _ => "  n: next match  N: prev match",
            };
            let q = q.clone();
            let col = 3u16 + q.chars().count() as u16;
            let line = Line::from(vec![
                Span::styled("-> ", Style::default().fg(app.colors.input_hint)),
                Span::raw(q),
                Span::raw("  "),
                Span::styled(
                    entry_status_text(app),
                    Style::default()
                        .fg(app.colors.status)
                        .add_modifier(Modifier::ITALIC),
                ),
                Span::styled(hint, Style::default().fg(app.colors.input_hint)),
            ]);
            (Text::from(line), format!("Search: {kind_label}"), Some(col))
        }
        Mode::LoginPassword { .. } | Mode::RecoveryKey { .. } => {
            let masked = mask_secret_input(&app.input.buffer);
            let col = 2u16 + app.input.buffer[..app.input.cursor].chars().count() as u16;
            let line = Line::from(vec![
                Span::raw("> "),
                Span::raw(masked),
                Span::raw("  "),
                Span::styled(
                    entry_status_text(app),
                    Style::default()
                        .fg(app.colors.status)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            let title = if matches!(app.mode, Mode::LoginPassword { .. }) {
                "Password".to_owned()
            } else {
                "Recovery key".to_owned()
            };
            (Text::from(line), title, Some(col))
        }
        Mode::ConfirmLogout { account } => {
            let line = Line::from(vec![
                Span::raw("> "),
                Span::styled(
                    format!("Log out {}? [y/N]", account.user_id),
                    Style::default()
                        .fg(app.colors.status)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            (Text::from(line), "Confirm logout".to_owned(), None)
        }
        Mode::SearchForm => {
            let line = Line::from(vec![
                Span::raw("> "),
                Span::styled(
                    app.search_form
                        .error
                        .clone()
                        .unwrap_or_else(|| "fill search fields".to_owned()),
                    Style::default()
                        .fg(app.colors.status)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            (Text::from(line), "Search".to_owned(), None)
        }
        Mode::SearchResults => {
            let mut status = app.search_result_status();
            let entry_status = entry_status_text(app);
            if !entry_status.is_empty() && entry_status != status {
                if status.is_empty() {
                    status = entry_status;
                } else {
                    status.push_str("  ");
                    status.push_str(&entry_status);
                }
            }
            let line = Line::from(vec![
                Span::raw("> "),
                Span::styled(
                    status,
                    Style::default()
                        .fg(app.colors.status)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            (Text::from(line), "Search Results".to_owned(), None)
        }
        _ => {
            let in_search_list = matches!(
                app.mode,
                Mode::RoomList | Mode::MessageList | Mode::AccountList
            ) && app.last_search.is_some();
            let input_text = if app.show_input_help && app.input.buffer.is_empty() {
                Span::styled(
                    "Type /help or /? for help",
                    Style::default()
                        .fg(app.colors.input_hint)
                        .add_modifier(Modifier::ITALIC),
                )
            } else {
                Span::raw(mask_login_command(&app.input.buffer))
            };
            let status_text = if command_response_will_popup
                || matches!(app.mode, Mode::Popup(PopupKind::CommandResponse))
            {
                String::new()
            } else {
                entry_status_text(app)
            };
            let status_span = Span::styled(
                status_text,
                Style::default()
                    .fg(app.colors.status)
                    .add_modifier(Modifier::ITALIC),
            );
            let mut trailing_hints = Vec::new();
            if in_search_list {
                trailing_hints.push(Span::styled(
                    "  n: next match  N: prev match",
                    Style::default().fg(app.colors.input_hint),
                ));
            }
            if let Some(hint) = search_command_entry_hint(&app.input.buffer) {
                trailing_hints.push(Span::styled(
                    format!("  {hint}"),
                    Style::default().fg(app.colors.input_hint),
                ));
            }
            // A multi-line compose buffer (Shift+Enter) renders one row per hard
            // line: the first carries the "> " prompt, continuations a "  " indent
            // to stay aligned, and the trailing status sits after the last line.
            let masked = mask_login_command(&app.input.buffer);
            let is_help = app.show_input_help && app.input.buffer.is_empty();
            let text = if is_help || !masked.contains('\n') {
                let mut spans = vec![Span::raw("> "), input_text, Span::raw("  "), status_span];
                spans.extend(trailing_hints);
                Text::from(Line::from(spans))
            } else {
                let segments: Vec<&str> = masked.split('\n').collect();
                let last = segments.len() - 1;
                let lines: Vec<Line> = segments
                    .into_iter()
                    .enumerate()
                    .map(|(i, segment)| {
                        let prefix = if i == 0 { "> " } else { "  " };
                        let mut spans = vec![Span::raw(prefix), Span::raw(segment.to_owned())];
                        if i == last {
                            spans.push(Span::raw("  "));
                            spans.push(status_span.clone());
                            spans.extend(trailing_hints.clone());
                        }
                        Line::from(spans)
                    })
                    .collect();
                Text::from(lines)
            };
            let col = if matches!(
                app.mode,
                Mode::Compose
                    | Mode::LoginUsername
                    | Mode::Editing { .. }
                    | Mode::Reacting { .. }
                    | Mode::DateJump
            ) && !is_help
            {
                Some(2u16 + app.input.buffer[..app.input.cursor].chars().count() as u16)
            } else {
                None
            };
            let title = match &app.mode {
                Mode::RoomList if app.last_search.is_some() => "Search: Rooms".to_owned(),
                Mode::MessageList if app.last_search.is_some() => "Search: Messages".to_owned(),
                Mode::AccountList if app.last_search.is_some() => "Search: Accounts".to_owned(),
                Mode::DateJump => "Jump to date".to_owned(),
                _ => String::new(),
            };
            (text, title, col)
        }
    };
    let input_active = matches!(
        app.mode,
        Mode::Compose
            | Mode::LoginUsername
            | Mode::LoginPassword { .. }
            | Mode::RecoveryKey { .. }
            | Mode::ConfirmLogout { .. }
            | Mode::Editing { .. }
            | Mode::Reacting { .. }
            | Mode::Unreacting { .. }
            | Mode::Search(_, _)
            | Mode::DateJump
            | Mode::SearchForm
            | Mode::SearchResults
    );
    let input_border = if input_active {
        Style::default()
            .fg(app.colors.selected_room)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.colors.border)
    };
    let input = Paragraph::new(command_line)
        .block(
            Block::default()
                .style(
                    Style::default()
                        .fg(app.colors.input_foreground)
                        .bg(app.colors.input_background),
                )
                .title(command_title)
                .borders(Borders::ALL)
                .border_type(if input_active {
                    BorderType::Double
                } else {
                    BorderType::Plain
                })
                .border_style(input_border),
        )
        .wrap(Wrap { trim: false });
    if app.mode == Mode::Compose && app.pending_command_response.is_some() {
        if command_response_will_popup {
            app.mode = Mode::Popup(PopupKind::CommandResponse);
            app.popup_scroll = 0;
            cursor_col = None;
        } else {
            app.pending_command_response = None;
        }
    }
    if let Some(col) = cursor_col {
        let inner_width = outer[1].width.saturating_sub(2) as usize;
        // In compose, the buffer can span multiple hard lines (Shift+Enter), so
        // resolve the cursor against the same layout the box renders; other input
        // modes are always a single (wrapping) logical line.
        let (vis_row, vis_col) = if app.mode == Mode::Compose {
            let (_, row, col) = compose_layout(&app.input.buffer, app.input.cursor, inner_width);
            (row, col)
        } else if inner_width > 0 && col as usize > inner_width {
            let overflow = col as usize - inner_width;
            (1 + overflow / inner_width, overflow % inner_width)
        } else {
            (0, col as usize)
        };
        let scroll_row = if vis_row >= effective_input_lines as usize {
            (vis_row + 1 - effective_input_lines as usize) as u16
        } else {
            0
        };
        let input = input.scroll((scroll_row, 0));
        frame.render_widget(input, outer[1]);
        frame.set_cursor_position((
            outer[1].x.saturating_add(1).saturating_add(vis_col as u16),
            outer[1]
                .y
                .saturating_add(1)
                .saturating_add((vis_row as u16).saturating_sub(scroll_row)),
        ));
    } else {
        frame.render_widget(input, outer[1]);
    }

    if app.mode == Mode::SearchResults
        || (app.mode == Mode::SearchForm && app.search_results.is_some())
    {
        render_search_results(frame, app, outer[0]);
    }

    if app.mode == Mode::SearchForm {
        render_search_form(frame, app, frame.area());
    }

    if app.mode == Mode::Verification {
        if let Some(flow) = app.verification.as_ref() {
            let area = centered_rect(72, 80, frame.area());
            frame.render_widget(Clear, area);
            let (title, lines) = verification_popup_view(flow);
            let popup = Paragraph::new(lines)
                .block(
                    Block::default()
                        .style(Style::default().bg(app.colors.popup_background))
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(app.colors.selected_room)),
                )
                .wrap(Wrap { trim: false });
            frame.render_widget(popup, area);
        }
    }

    if let Mode::Popup(PopupKind::MediaPreview) = app.mode {
        if let Some(rect) = render_media_preview(frame, app, frame.area()) {
            frame_image_rects.push(rect);
        }
    } else if let Mode::Popup(kind) = app.mode {
        let area = blocking_popup_area(app, frame.area()).unwrap_or_else(|| frame.area());
        let command_response = app.pending_command_response.as_deref().unwrap_or_default();
        frame.render_widget(Clear, area);
        let page_size = usize::from(area.height.saturating_sub(2)).max(1);
        let (popup_title, lines) = match kind {
            PopupKind::Help => {
                let lines = popup_help_lines(app);
                let sel_line = help_line_of_selection(app.help_selection);
                if sel_line < app.popup_scroll {
                    app.popup_scroll = sel_line;
                } else if sel_line >= app.popup_scroll.saturating_add(page_size) {
                    app.popup_scroll = sel_line.saturating_add(1).saturating_sub(page_size);
                }
                ("Help  (Enter to select, Esc to close)", lines)
            }
            PopupKind::Shortcuts => (
                "Shortcuts  (Esc to close)",
                popup_shortcuts_lines(&app.shortcuts),
            ),
            PopupKind::UnreadThreads => {
                let entries = app.unread_thread_entries();
                app.sync_unread_thread_selection(&entries);
                let (lines, ranges) = popup_unread_thread_lines(
                    app,
                    &entries,
                    usize::from(area.width.saturating_sub(2)),
                );
                let entries_len = ranges.len();
                if entries_len == 0 {
                    app.unread_thread_selection = 0;
                    app.unread_thread_selected = None;
                } else {
                    app.sync_unread_thread_selection(&entries);
                }
                if let Some(range) = ranges.get(app.unread_thread_selection) {
                    if range.start < app.popup_scroll {
                        app.popup_scroll = range.start;
                    } else if range.end > app.popup_scroll.saturating_add(page_size) {
                        app.popup_scroll = range.end.saturating_sub(page_size);
                    }
                }
                ("Unread Threads  (Enter to open, Esc to close)", lines)
            }
            PopupKind::RoomInfo => (
                "Room Info  (Esc to close, Up/Down scroll)",
                popup_room_info_lines(app)
                    .into_iter()
                    .map(Line::from)
                    .collect(),
            ),
            PopupKind::Status => (
                "Status  (Esc to close)",
                popup_status_lines(app)
                    .into_iter()
                    .map(Line::from)
                    .collect(),
            ),
            PopupKind::CommandResponse => (
                "Command Response  (Esc to close)",
                wrap_command_response(command_response, area.width.saturating_sub(2))
                    .into_iter()
                    .map(Line::from)
                    .collect(),
            ),
            PopupKind::MediaPreview => unreachable!("media preview handled above"),
        };
        let max_scroll = lines.len().saturating_sub(page_size);
        app.popup_scroll = app.popup_scroll.min(max_scroll);
        let visible_lines = lines
            .into_iter()
            .skip(app.popup_scroll)
            .take(page_size)
            .collect::<Vec<_>>();
        let popup = Paragraph::new(visible_lines)
            .block(
                Block::default()
                    .style(Style::default().bg(app.colors.popup_background))
                    .title(popup_title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(app.colors.selected_room)),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(popup, area);
    }

    // Erase pixel-protocol ghosts: any cell an image occupied last frame but
    // not this one. Halfblock images are ordinary glyphs already caught by the
    // normal cell-diff, so skip the work entirely for that protocol.
    if !matches!(app.picker.protocol_type(), ProtocolType::Halfblocks) {
        clear_image_ghosts(frame, &app.prev_image_rects, &frame_image_rects);
    }
    app.prev_image_rects = frame_image_rects;
}

/// Force-repaint cells that held a pixel-protocol image last frame but are no
/// longer covered by one. Sixel/iTerm2 pixels are outside ratatui's cell model,
/// so a vacated, still-blank cell would otherwise keep its stale pixels: both
/// the old and new buffer cells compare equal and the diff emits nothing.
/// Marking such cells `AlwaysUpdate` forces a terminal write that clears them.
fn clear_image_ghosts(frame: &mut Frame<'_>, prev: &[Rect], current: &[Rect]) {
    let buf = frame.buffer_mut();
    for area in prev {
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                // Still covered by an image this frame — leave its Skip cells be.
                if current.iter().any(|r| r.contains((x, y).into())) {
                    continue;
                }
                if let Some(c) = buf.cell_mut((x, y)) {
                    // Only touch blank cells with no diff decision yet. Cells
                    // holding text/halfblocks (symbol != " ") are already caught
                    // by the normal diff; cells a fresh image set to Skip must
                    // not be overwritten or we'd paint over the new image.
                    if c.symbol() == " " && c.diff_option == CellDiffOption::None {
                        c.set_diff_option(CellDiffOption::AlwaysUpdate);
                    }
                }
            }
        }
    }
}

fn render_search_form(frame: &mut Frame<'_>, app: &App, screen: Rect) {
    let lines = search_form_lines(app);
    let caption = search_form_caption();
    let area = search_form_area(&lines, &caption, screen);
    frame.render_widget(Clear, area);
    let popup = Paragraph::new(lines)
        .block(
            Block::default()
                .style(Style::default().bg(app.colors.popup_background))
                .title("Search")
                .title_bottom(caption)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.colors.selected_room)),
        )
        .style(Style::default().bg(app.colors.popup_background))
        .wrap(Wrap { trim: false });
    frame.render_widget(popup, area);
}

fn search_form_area(lines: &[Line<'static>], caption: &Line<'static>, screen: Rect) -> Rect {
    let content_width = lines
        .iter()
        .map(line_width)
        .chain(std::iter::once(line_width(caption)))
        .max()
        .unwrap_or(40);
    let max_width = screen.width.saturating_sub(2).max(1) as usize;
    let min_width = 42.min(max_width);
    let width = content_width.saturating_add(2).clamp(min_width, max_width) as u16;
    let height = (lines.len() as u16)
        .saturating_add(2)
        .min(screen.height.saturating_sub(2).max(3));
    centered_size(width.min(screen.width), height, screen)
}

fn line_width(line: &Line<'_>) -> usize {
    line.width()
}

fn search_form_caption() -> Line<'static> {
    Line::from(vec![
        Span::styled(" Tab/Down ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("next  "),
        Span::styled(
            " Shift+Tab/Up ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("previous  "),
        Span::styled(
            " Left/Right/Space ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("scope  "),
        Span::styled(" Enter ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("search  "),
        Span::styled(" Esc ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("cancel "),
    ])
}

fn search_form_lines(app: &App) -> Vec<Line<'static>> {
    let form = &app.search_form;
    let mut lines = vec![search_form_field_line(
        "Query",
        form.query.as_str(),
        "optional with filters",
        form.field == SearchFormField::Query,
        true,
        app,
    )];
    lines.push(search_scope_line(
        &form.scope,
        form.field == SearchFormField::Scope,
        app,
    ));
    if form.field_is_visible(&SearchFormField::Room) {
        lines.push(search_form_field_line(
            "Room",
            form.room.as_str(),
            "room name or id",
            form.field == SearchFormField::Room,
            true,
            app,
        ));
    }
    if form.field_is_visible(&SearchFormField::Account) {
        let placeholder = match form.scope {
            SearchScope::SpecificRoom => "optional account filter",
            SearchScope::SpecificAccount => "account name or id",
            SearchScope::CurrentRoom | SearchScope::CurrentAccount | SearchScope::All => "",
        };
        lines.push(search_form_field_line(
            "Account",
            form.account.as_str(),
            placeholder,
            form.field == SearchFormField::Account,
            true,
            app,
        ));
    }
    lines.extend([
        search_form_field_line(
            "Sender",
            form.sender.as_str(),
            "any sender",
            form.field == SearchFormField::Sender,
            true,
            app,
        ),
        search_form_field_line(
            "Date",
            form.date.as_str(),
            "any date or range",
            form.field == SearchFormField::Date,
            true,
            app,
        ),
        search_form_field_line(
            "After",
            form.after.as_str(),
            "no lower bound",
            form.field == SearchFormField::After,
            true,
            app,
        ),
        search_form_field_line(
            "Before",
            form.before.as_str(),
            "no upper bound",
            form.field == SearchFormField::Before,
            true,
            app,
        ),
        search_form_field_line(
            "Limit",
            form.limit.as_str(),
            "result limit",
            form.field == SearchFormField::Limit,
            true,
            app,
        ),
    ]);
    if let Some(error) = form.error.as_deref() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_owned(),
            Style::default().fg(Color::Red),
        )));
    }
    lines
}

fn search_form_field_line(
    label: &str,
    value: &str,
    placeholder: &str,
    active: bool,
    editable: bool,
    app: &App,
) -> Line<'static> {
    let marker = if active { ">" } else { " " };
    let label_style = if active {
        Style::default()
            .fg(app.colors.selected_room)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let mut spans = vec![
        Span::raw(format!("{marker} ")),
        Span::styled(format!("{label:<8} "), label_style),
    ];
    if value.is_empty() {
        if active && editable {
            spans.push(Span::styled(
                "▌",
                Style::default()
                    .fg(app.colors.selected_room)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(
            placeholder.to_owned(),
            Style::default().fg(app.colors.input_hint),
        ));
    } else {
        spans.push(Span::raw(value.to_owned()));
        if active && editable {
            spans.push(Span::styled(
                "▌",
                Style::default()
                    .fg(app.colors.selected_room)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    Line::from(spans)
}

fn search_scope_line(scope: &SearchScope, active: bool, app: &App) -> Line<'static> {
    let marker = if active { ">" } else { " " };
    let label_style = if active {
        Style::default()
            .fg(app.colors.selected_room)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let mut spans = vec![
        Span::raw(format!("{marker} ")),
        Span::styled("Search in ", label_style),
    ];
    for (index, candidate) in [
        SearchScope::CurrentRoom,
        SearchScope::CurrentAccount,
        SearchScope::All,
        SearchScope::SpecificRoom,
        SearchScope::SpecificAccount,
    ]
    .into_iter()
    .enumerate()
    {
        if index > 0 {
            spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        }
        let selected = *scope == candidate;
        let style = if selected {
            Style::default()
                .fg(app.colors.selected_room)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.colors.input_hint)
        };
        let label = if selected {
            format!("[{}]", search_scope_label(&candidate))
        } else {
            search_scope_label(&candidate).to_owned()
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

fn search_scope_label(scope: &SearchScope) -> &'static str {
    match scope {
        SearchScope::CurrentRoom => "current room",
        SearchScope::CurrentAccount => "this account",
        SearchScope::All => "all accounts",
        SearchScope::SpecificRoom => "specific room",
        SearchScope::SpecificAccount => "specific account",
    }
}

fn render_search_results(frame: &mut Frame<'_>, app: &App, screen: Rect) {
    let area = screen;
    if area.width < 3 || area.height < 3 {
        return;
    }
    frame.render_widget(Clear, area);
    let lines = search_results_lines(
        app,
        area.width.saturating_sub(2) as usize,
        area.height.saturating_sub(2) as usize,
    );
    let popup = Paragraph::new(lines)
        .block(
            Block::default()
                .style(Style::default().bg(app.colors.popup_background))
                .title("Search Results")
                .title_bottom(search_results_caption(app))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.colors.selected_room)),
        )
        .style(Style::default().bg(app.colors.popup_background))
        .wrap(Wrap { trim: false });
    frame.render_widget(popup, area);
}

fn search_results_caption(app: &App) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {} ", app.shortcuts.submit.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("jump  "),
        Span::styled(
            format!(" {} ", app.shortcuts.search_sort.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("sort  "),
        Span::styled(
            format!(" {} ", app.shortcuts.search_group.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("group  "),
        Span::styled(
            format!(" {} ", app.shortcuts.search_edit.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("edit  "),
        Span::styled(
            format!(" {} ", app.shortcuts.reply.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("reply  "),
        Span::styled(
            format!(" {} ", app.shortcuts.thread.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("thread  "),
        Span::styled(
            format!(" {} ", app.shortcuts.clear_input.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("close "),
    ])
}

fn search_request_label(request: &crate::search::SearchRequest) -> String {
    if request.q.trim().is_empty() {
        "filter-only search  ".to_owned()
    } else {
        format!("\"{}\"  ", request.q)
    }
}

fn search_results_lines(app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
    let Some(state) = app.search_results.as_ref() else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            search_request_label(&state.request),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(app.search_result_status(), Style::default().fg(Color::Gray)),
        Span::styled(
            format!(
                "  sort: {}  group: {}",
                state.sort_order.label(),
                state.grouping.label()
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(""));
    if state.results.is_empty() {
        lines.push(Line::from("No results."));
        return lines;
    }

    let ordered = state.ordered_indices();
    let selected_position = ordered
        .iter()
        .position(|index| *index == state.selected)
        .unwrap_or(0);
    let blocks = search_result_blocks(app, state, width, &ordered);
    let content_budget = height.saturating_sub(2).max(1);
    let (start, end) = search_visible_result_range(&blocks, selected_position, content_budget);
    for block in blocks[start..end].iter().cloned() {
        lines.extend(block);
    }
    if state.loading {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Loading more results...",
            Style::default().fg(Color::Gray),
        )));
    }
    lines
}

fn search_result_blocks(
    app: &App,
    state: &SearchResultsState,
    width: usize,
    ordered: &[usize],
) -> Vec<Vec<Line<'static>>> {
    let mut blocks = Vec::with_capacity(ordered.len());
    let mut previous_date = None;
    let mut previous_room = None;
    for (position, result_index) in ordered.iter().copied().enumerate() {
        let mut block = Vec::new();
        let result = &state.results[result_index];
        let selected = result_index == state.selected;
        let event = &result.event;
        let room_key = (event.account_id, event.room_id.clone());
        if state.grouping == SearchGrouping::Room && previous_room.as_ref() != Some(&room_key) {
            previous_room = Some(room_key);
            previous_date = None;
            block.push(Line::from(vec![
                Span::styled("Room: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    search_event_room_label(app, event),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        let date = format_date(event.origin_ts);
        if previous_date.as_deref() != Some(date.as_str()) {
            previous_date = Some(date.clone());
            block.push(date_separator_line(&date, width, &app.colors));
        }
        let marker = if selected { ">" } else { " " };
        let title_style = if selected {
            Style::default()
                .fg(app.colors.selected_room)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        block.push(
            Line::from(vec![
                Span::raw(format!("{marker} {:>3}. ", position + 1)),
                Span::styled(
                    format_time(event.origin_ts, app.display.time_format),
                    Style::default().fg(Color::Gray),
                ),
                Span::raw(" "),
                Span::styled(search_event_room_label(app, event), title_style),
                Span::raw(" "),
                Span::styled(app.sender_label(event), Style::default().fg(Color::Gray)),
                Span::styled(
                    format!("  {:.2}", result.score),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
            .style(selected_line_style(
                &app.colors,
                selected,
                app.display.highlight_selected_line,
            )),
        );

        if selected {
            let key = SearchContextKey::from_event(event);
            if let Some(context) = state.context_cache.get(&key) {
                for context_event in &context.events {
                    let is_hit = context_event.event_id == event.event_id;
                    let prefix = if is_hit { "    > " } else { "      " };
                    let style = if is_hit {
                        Style::default().fg(app.colors.selected_room)
                    } else {
                        Style::default()
                    };
                    let body_width = width.saturating_sub(prefix.len()).max(20);
                    let body = truncate_chars(&context_event.display_body(), body_width);
                    block.push(
                        Line::from(vec![
                            Span::styled(prefix, style),
                            Span::styled(
                                format_time(context_event.origin_ts, app.display.time_format),
                                Style::default().fg(Color::Gray),
                            ),
                            Span::raw(" "),
                            Span::styled(
                                app.sender_label(context_event),
                                Style::default().fg(Color::Gray),
                            ),
                            Span::raw(": "),
                            Span::styled(body, style),
                        ])
                        .style(selected_line_style(
                            &app.colors,
                            is_hit,
                            app.display.highlight_selected_line,
                        )),
                    );
                }
            } else {
                block.push(Line::from(Span::styled(
                    "      loading context...",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        } else {
            let body_width = width.saturating_sub(8).max(20);
            block.push(Line::from(vec![
                Span::raw("      "),
                Span::raw(truncate_chars(&event.display_body(), body_width)),
            ]));
        }
        blocks.push(block);
    }
    blocks
}

fn search_visible_result_range(
    blocks: &[Vec<Line<'static>>],
    selected_position: usize,
    budget: usize,
) -> (usize, usize) {
    if blocks.is_empty() {
        return (0, 0);
    }
    let selected_position = selected_position.min(blocks.len() - 1);
    let mut start = selected_position;
    let mut end = selected_position + 1;
    let mut used = blocks[selected_position].len().max(1);
    while start > 0 {
        let prev_len = blocks[start - 1].len().max(1);
        if used + prev_len > budget {
            break;
        }
        start -= 1;
        used += prev_len;
    }
    while end < blocks.len() {
        let next_len = blocks[end].len().max(1);
        if used + next_len > budget {
            break;
        }
        end += 1;
        used += next_len;
    }
    (start, end)
}

fn search_event_room_label(app: &App, event: &crate::api::EventDto) -> String {
    app.rooms
        .rooms
        .iter()
        .find(|room| room.account_id == event.account_id && room.room_id == event.room_id)
        .map(RoomDto::title)
        .unwrap_or(event.room_id.as_str())
        .to_owned()
}

fn truncate_chars(value: &str, max_width: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if display_width(&value) <= max_width {
        return value;
    }
    if max_width <= 3 {
        return take_display_width(&value, max_width);
    }
    let mut out = take_display_width(&value, max_width - 3);
    out.push_str("...");
    out
}

fn display_width(value: &str) -> usize {
    value.chars().map(|ch| ch.width_cjk().unwrap_or(1)).sum()
}

fn take_display_width(value: &str, max_width: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in value.chars() {
        let ch_width = ch.width_cjk().unwrap_or(1);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out
}

fn image_thumbnail_spec(
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

fn thumbnail_overlaps_blocking_popup(rect: Rect, popup_area: Option<Rect>) -> bool {
    popup_area.is_some_and(|area| rect.intersects(area))
}

/// The sub-rect a pixel-protocol image actually paints: anchored at `rect`'s
/// top-left and clamped to the image's own aspect-fit cell `size`. The image
/// widget never fills the whole reserved `rect`, so popup-collision and ghost
/// tracking must use this narrower rect, not `rect` itself.
fn image_draw_rect(rect: Rect, size: Size) -> Rect {
    Rect::new(
        rect.x,
        rect.y,
        size.width.min(rect.width),
        size.height.min(rect.height),
    )
}

/// The screen region occupied by an open modal popup, or `None` when no modal is
/// active.  Every `Mode::Popup` variant is modal and must sit above background
/// refreshes, so the area is computed identically to where each popup is drawn
/// (see the `Mode::Popup` rendering block above).
fn blocking_popup_area(app: &App, screen: Rect) -> Option<Rect> {
    match app.mode {
        Mode::Popup(PopupKind::MediaPreview) => Some(media_preview_modal_area(app, screen)),
        Mode::Popup(PopupKind::CommandResponse) => {
            let command_response = app.pending_command_response.as_deref().unwrap_or_default();
            Some(command_response_popup_area(command_response, screen))
        }
        Mode::Popup(_) => Some(centered_rect(72, 80, screen)),
        _ => None,
    }
}

/// Compute the cell dimensions at which an image should be encoded for the
/// media-preview popup on a screen of the given size.  Returns `None` when the
/// image has zero natural dimensions (shouldn't happen for a valid decode).
fn preview_target_size(
    img: &image::DynamicImage,
    font_size: FontSize,
    screen: Rect,
) -> Option<Size> {
    let max_area = centered_rect(PREVIEW_MAX_PCT, PREVIEW_MAX_PCT, screen);
    // Subtract the 1-cell border on each side (same as Block::inner).
    let max_w = max_area.width.saturating_sub(2);
    let max_h = max_area.height.saturating_sub(2);
    let nat = Resize::natural_size(img, font_size);
    if nat.width == 0 || nat.height == 0 {
        return None;
    }
    let scale = (max_w as f32 / nat.width as f32)
        .min(max_h as f32 / nat.height as f32)
        .min(1.0);
    Some(Size::new(
        ((nat.width as f32 * scale) as u16).max(1),
        ((nat.height as f32 * scale) as u16).max(1),
    ))
}

/// Returns the rect the image widget was drawn into, when one was actually
/// rendered (used to track pixel-protocol ghosts); `None` for the loading,
/// failure, and "no image" placeholder states that draw only text.
/// Cell layout of the media-preview modal for the current selection and decoded
/// image state: `(area, target_size, caption_h)`, where `area` is the bordered
/// modal rect and the other two size the image and caption inside it. The modal
/// shrinks to fit the image, falling back to the 88% max while the image is still
/// loading or larger than the cap. Shared by the renderer and
/// [`media_preview_modal_area`] so thumbnail suppression matches exactly where the
/// modal is drawn (using the 88% max would hide thumbnails the modal never covers).
fn media_preview_layout(
    app: &App,
    screen: Rect,
    caption: Option<&str>,
    media: &MediaKey,
) -> (Rect, Size, u16) {
    let max_area = centered_rect(PREVIEW_MAX_PCT, PREVIEW_MAX_PCT, screen);
    let max_inner = Block::default().borders(Borders::ALL).inner(max_area);
    let font_size = app.picker.font_size();
    let target_size = match app.image_cache.get(media) {
        Some(ImageState::Ready(img)) => preview_target_size(img, font_size, screen)
            .unwrap_or_else(|| Size::new(max_inner.width, max_inner.height)),
        _ => Size::new(max_inner.width, max_inner.height),
    };
    // Reserve lines below the image for the caption text.
    let caption_h = caption
        .map(|c| {
            let w = (target_size.width as usize).max(1);
            wrap_rich_lines(plain_rich_lines(c), w, w).len() as u16
        })
        .unwrap_or(0);
    let (target_size, caption_h) = fit_preview_caption(target_size, caption_h, max_inner);
    // Compute the popup area from target_size now — before we know whether the
    // protocol is ready — so the border never jumps when encoding finishes.
    let content_h = target_size.height.saturating_add(caption_h);
    let area = if target_size.width < max_inner.width || content_h < max_inner.height {
        // Add 1-cell border on each side and center with the same helper used
        // everywhere else, avoiding independent centering arithmetic here.
        centered_size(target_size.width + 2, content_h + 2, screen)
    } else {
        max_area
    };
    (area, target_size, caption_h)
}

/// The screen rect the media-preview modal occupies, for thumbnail-suppression.
/// Falls back to the 88% max when the selected message has no image (matching the
/// "no image" placeholder, which fills `max_area`).
fn media_preview_modal_area(app: &App, screen: Rect) -> Rect {
    let max_area = centered_rect(PREVIEW_MAX_PCT, PREVIEW_MAX_PCT, screen);
    let Some((media, caption)) = app.selected_message_event().and_then(|event| {
        event.image_mxc().map(|(account_id, mxc_url)| {
            (MediaKey::new(account_id, mxc_url), event.image_caption())
        })
    }) else {
        return max_area;
    };
    media_preview_layout(app, screen, caption.as_deref(), &media).0
}

fn render_media_preview(frame: &mut Frame<'_>, app: &mut App, screen: Rect) -> Option<Rect> {
    let border_style = Style::default().fg(app.colors.selected_room);

    let selected = app.selected_message_event().and_then(|event| {
        event.image_mxc().map(|(account_id, mxc_url)| {
            (
                MediaKey::new(account_id, mxc_url),
                event.image_is_encrypted(),
                event.image_filename(),
                event.image_caption(),
            )
        })
    });

    let max_area = centered_rect(PREVIEW_MAX_PCT, PREVIEW_MAX_PCT, screen);

    let Some((media, encrypted, filename, caption)) = selected else {
        let block = Block::default()
            .title("Image Preview  (Esc to close)")
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = block.inner(max_area);
        frame.render_widget(Clear, max_area);
        frame.render_widget(block, max_area);
        frame.render_widget(Paragraph::new("Selected message has no image."), inner);
        return None;
    };

    let title = filename
        .as_deref()
        .map(|n| format!("{n}  (Esc to close)"))
        .unwrap_or_else(|| "Image Preview  (Esc to close)".to_owned());

    app.request_image(media.account_id, media.mxc_url.clone(), encrypted);

    // Size the modal to the image (shared with blocking_popup_area so thumbnail
    // suppression matches exactly where the modal is drawn).
    let (area, target_size, caption_h) =
        media_preview_layout(app, screen, caption.as_deref(), &media);
    let block = Block::default()
        .title(title.as_str())
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    // Split inner into image area (top) and caption area (bottom).
    let image_area = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        target_size.height.min(inner.height),
    );
    let caption_area = (caption_h > 0 && inner.height > target_size.height).then(|| {
        Rect::new(
            inner.x,
            inner.y + target_size.height,
            inner.width,
            caption_h.min(inner.height - target_size.height),
        )
    });

    let protocol_key = ProtocolKey {
        media: media.clone(),
        size: target_size,
    };
    app.request_protocol(media.clone(), target_size);

    match app.image_cache.get(&media) {
        Some(ImageState::Failed(error)) => {
            frame.render_widget(
                Paragraph::new(format!("Image unavailable: {error}")).wrap(Wrap { trim: false }),
                inner,
            );
            None
        }
        Some(ImageState::Ready(_)) => {
            let drawn = match app.proto_cache.get(&protocol_key) {
                Some(ProtocolState::Ready(protocol)) => {
                    frame.render_widget(
                        Image::new(protocol.preview(app.sixel_preview_generation)),
                        image_area,
                    );
                    Some(image_area)
                }
                Some(ProtocolState::Failed(error)) => {
                    frame.render_widget(
                        Paragraph::new(format!("Unable to render image: {error}"))
                            .wrap(Wrap { trim: false }),
                        image_area,
                    );
                    None
                }
                _ => {
                    frame.render_widget(Paragraph::new("Preparing image..."), image_area);
                    None
                }
            };
            if let (Some(cap), Some(crect)) = (caption.as_deref(), caption_area) {
                frame.render_widget(
                    Paragraph::new(cap.to_owned())
                        .alignment(ratatui::layout::Alignment::Center)
                        .wrap(Wrap { trim: false }),
                    crect,
                );
            }
            drawn
        }
        _ => {
            frame.render_widget(Paragraph::new("Loading image..."), inner);
            None
        }
    }
}

fn fit_preview_caption(target: Size, caption_h: u16, bounds: Rect) -> (Size, u16) {
    if caption_h == 0 || bounds.height == 0 {
        return (target, 0);
    }
    let caption_h = caption_h.min(bounds.height.saturating_sub(1));
    let image_h = target
        .height
        .min(bounds.height.saturating_sub(caption_h))
        .max(1);
    (
        Size::new(target.width.min(bounds.width), image_h),
        caption_h,
    )
}
fn mask_login_command(input: &str) -> String {
    let trimmed = input.trim_start();
    let leading_len = input.len() - trimmed.len();
    let Some(rest) = trimmed.strip_prefix("/login") else {
        return input.to_owned();
    };
    let Some(first_space) = rest.find(char::is_whitespace) else {
        return input.to_owned();
    };
    let after_command = &rest[first_space..];
    let credentials = after_command.trim_start();
    let Some(username_end) = credentials.find(char::is_whitespace) else {
        return input.to_owned();
    };
    let password_start = leading_len + trimmed.len() - credentials.len() + username_end;
    let prefix = &input[..password_start];
    let password = &input[password_start..];
    format!(
        "{prefix}{}",
        password
            .chars()
            .map(|ch| if ch.is_whitespace() { ch } else { '•' })
            .collect::<String>()
    )
}

fn mask_secret_input(input: &str) -> String {
    "•".repeat(input.chars().count())
}

#[cfg(test)]
mod recovery_tests {
    use super::mask_secret_input;

    #[test]
    fn secret_prompt_masks_every_character() {
        assert_eq!(
            mask_secret_input("secret recovery key"),
            "•••••••••••••••••••"
        );
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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

fn centered_size(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

/// Build the title and body for the SAS verification modal (ADR 0028 §2): the
/// seven emoji with descriptions, the decimal triple fallback, the current
/// stage, and a `[y]es / [n]o · Esc` prompt — plus distinct terminal states.
fn verification_popup_view(flow: &VerificationFlow) -> (&'static str, Vec<Line<'static>>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let direction = match flow.direction {
        VerificationDirection::Incoming => "Incoming verification request",
        VerificationDirection::Outgoing => "Verifying",
    };
    // Name whichever target the flow carries: a user (cross-user, ADR 0040), a
    // device (self-verification), or both once known.
    let target = match (flow.user_id.as_str(), flow.device_id.as_str()) {
        (user, device) if !user.is_empty() && !device.is_empty() => {
            format!("{user} (device {device})")
        }
        (user, _) if !user.is_empty() => user.to_owned(),
        (_, device) if !device.is_empty() => format!("device {device}"),
        _ => "…".to_owned(),
    };
    lines.push(Line::from(Span::styled(
        format!("{direction}: {target}"),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    if let Some(fid) = flow.flow_id.as_deref() {
        lines.push(Line::from(Span::styled(
            format!("flow: {fid}"),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
    }

    match &flow.stage {
        VerificationStage::Starting | VerificationStage::Waiting => {
            lines.push(Line::from("Waiting for the other device…"));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[r] resync from server  ·  [n]/Esc to cancel",
                Style::default().fg(Color::DarkGray),
            )));
        }
        VerificationStage::Compare => {
            lines.push(Line::from(
                "Compare these emoji with the other device. Do they match?",
            ));
            lines.push(Line::from(""));
            if let Some(emoji) = flow.emoji.as_ref() {
                for pair in emoji {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {}  ", pair.symbol),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(pair.description.clone()),
                    ]));
                }
            }
            if let Some([a, b, c]) = flow.decimals {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  Decimal fallback: {a} - {b} - {c}"),
                    Style::default().fg(Color::Gray),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[y]es, they match  /  [n]o  ·  Esc to cancel",
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }
        VerificationStage::Confirming => {
            lines.push(Line::from(
                "You confirmed. Waiting for the other device to confirm…",
            ));
        }
        VerificationStage::Done => {
            lines.push(Line::from(Span::styled(
                "✓ Verification complete.",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("Press Esc to close."));
        }
        VerificationStage::Ended(message) => {
            lines.push(Line::from(Span::styled(
                message.clone(),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("Press Esc to close."));
        }
    }

    ("Verification", lines)
}

/// Returns the rendered line index of the nth `HELP_COMMANDS` entry,
/// accounting for the section-header and blank lines inserted between groups.
fn help_line_of_selection(selection: usize) -> usize {
    // Each group after the first inserts a blank line + header line (2 lines).
    // The first group inserts just the header (1 line, no leading blank).
    let extra: usize = HELP_COMMAND_GROUPS
        .iter()
        .filter(|(start, _)| *start <= selection)
        .map(|(start, _)| if *start == 0 { 1 } else { 2 })
        .sum();
    selection + extra
}

fn popup_help_lines(app: &App) -> Vec<Line<'static>> {
    let header_style = Style::default()
        .fg(app.colors.selected_room)
        .add_modifier(Modifier::BOLD);
    let mut lines: Vec<Line<'static>> =
        Vec::with_capacity(HELP_COMMANDS.len() + HELP_COMMAND_GROUPS.len() * 2);
    let mut group_iter = HELP_COMMAND_GROUPS.iter().peekable();

    for (index, command) in HELP_COMMANDS.iter().enumerate() {
        if group_iter.peek().map(|(i, _)| *i) == Some(index) {
            let (_, title) = group_iter.next().unwrap();
            if index > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(*title, header_style)));
        }

        let marker = if index == app.help_selection {
            ">"
        } else {
            " "
        };
        let row_style = if index == app.help_selection {
            Style::default()
                .fg(app.colors.selected_room)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {:<40}", command.label), row_style),
            Span::raw(format!("  {}", command.description)),
        ]));
    }
    lines
}

const UNREAD_THREAD_PREVIEW_LINES: usize = 3;

fn popup_unread_thread_lines(
    app: &App,
    entries: &[UnreadThreadEntry],
    width: usize,
) -> (Vec<Line<'static>>, Vec<Range<usize>>) {
    if entries.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "No unread threads",
                Style::default().fg(app.colors.input_hint),
            ))],
            Vec::new(),
        );
    }
    let mut lines = Vec::with_capacity(entries.len().saturating_mul(2));
    let mut ranges = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let start = lines.len();
        let selected = index == app.unread_thread_selection;
        let marker = if selected { ">" } else { " " };
        let marker_style = if selected {
            Style::default()
                .fg(app.colors.selected_room)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let unread_style = Style::default()
            .fg(app.colors.unread_count)
            .add_modifier(Modifier::BOLD);
        let root = entry
            .root_snippet
            .as_deref()
            .filter(|snippet| !snippet.trim().is_empty())
            .map(|snippet| format!(" — {}", compact_popup_text(snippet, 54)))
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} "), marker_style),
            Span::styled(
                compact_popup_text(&entry.room_title, 34),
                if selected {
                    Style::default()
                        .fg(app.colors.selected_room)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                },
            ),
            Span::styled(format!("  {} new", entry.unread_count), unread_style),
            Span::styled(root, Style::default().fg(app.colors.input_hint)),
        ]));
        let previews = if entry.recent.is_empty() {
            vec![(entry.latest_sender.as_str(), entry.latest_body.as_str())]
        } else {
            entry
                .recent
                .iter()
                .map(|preview| (preview.sender.as_str(), preview.body.as_str()))
                .collect::<Vec<_>>()
        };
        let mut remaining = UNREAD_THREAD_PREVIEW_LINES;
        let mut preview_groups: Vec<Vec<Line<'static>>> = Vec::new();
        for (sender, body) in previews {
            if remaining == 0 {
                break;
            }
            let wrapped =
                popup_preview_lines(sender, body, width, remaining, app.colors.input_hint);
            remaining = remaining.saturating_sub(wrapped.len());
            preview_groups.push(wrapped);
        }
        for group in preview_groups.into_iter().rev() {
            lines.extend(group);
        }
        ranges.push(start..lines.len());
    }
    (lines, ranges)
}

fn popup_preview_lines(
    sender: &str,
    body: &str,
    width: usize,
    limit: usize,
    style: Color,
) -> Vec<Line<'static>> {
    let text = format!(
        "  {}: {}",
        compact_popup_text(sender, 24),
        body.replace(['\n', '\r'], " ")
    );
    rich_lines_to_spans(wrap_rich_lines(
        plain_rich_lines(&text),
        width.max(1),
        width.max(1),
    ))
    .into_iter()
    .take(limit)
    .map(|spans| {
        let text = spans
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        Line::from(Span::styled(text, Style::default().fg(style)))
    })
    .collect()
}

fn compact_popup_text(text: &str, max: usize) -> String {
    let flattened = text.replace(['\n', '\r'], " ");
    let trimmed = flattened.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    let head: String = trimmed.chars().take(max).collect();
    format!("{}…", head.trim_end())
}

pub(crate) fn popup_room_info_lines(app: &App) -> Vec<String> {
    let Some(room) = app.selected_room() else {
        return vec!["No room selected.".to_owned()];
    };
    let aliases = room
        .canonical_alias
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| "unavailable (API support needed for alias list)".to_owned());
    let account_user_id = room
        .account_user_id
        .as_deref()
        .unwrap_or("unavailable from room summary");
    let avatar = room.avatar_url.as_deref().unwrap_or("none");
    let topic = room.topic.as_deref().unwrap_or("none");
    let last_event = room.last_event_id.as_deref().unwrap_or("none");
    let mut lines = vec![
        format!("Name: {}", room.title()),
        format!("Matrix ID: {}", room.room_id),
        format!("Account ID: {}", room.account_id),
        format!("Your Matrix ID: {account_user_id}"),
        format!("Aliases: {aliases}"),
        format!("Topic: {topic}"),
        format!("Avatar: {avatar}"),
        format!(
            "Last activity: {}",
            format_time(room.last_activity_ts, app.display.time_format)
        ),
        format!("Last event: {last_event}"),
        "Encryption: unavailable (API support needed)".to_owned(),
        "Access: unavailable (API support needed)".to_owned(),
        "Room type/version: unavailable (API support needed)".to_owned(),
        "".to_owned(),
        "Members from loaded timeline:".to_owned(),
    ];

    // For an unnamed room (e.g. a DM), the `Name:` line above is the raw room id.
    // Surface the member-derived display name too, once one has been resolved.
    if let Some(dm_name) = app.room_titles.get(&RoomKey::from(room)) {
        lines.insert(1, format!("DM name: {dm_name}"));
    }

    let members = known_room_members(app);
    if members.is_empty() {
        lines.push("  unavailable (API support needed for complete room members)".to_owned());
    } else {
        lines.extend(members.into_iter().map(|member| {
            let display_name = member
                .display_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "unknown".to_owned());
            format!(
                "  {display_name}  {}  ({})",
                member.user_id, member.membership
            )
        }));
        lines.push("".to_owned());
        lines.push(
            "Complete member list requires API support; this list only reflects loaded timeline state."
                .to_owned(),
        );
    }
    lines
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KnownRoomMember {
    user_id: String,
    display_name: Option<String>,
    membership: String,
}

fn known_room_members(app: &App) -> Vec<KnownRoomMember> {
    let mut by_user = HashMap::<String, KnownRoomMember>::new();
    for member in app
        .selected_raw_events()
        .iter()
        .filter(|event| event.event_type == "m.room.member")
        .filter_map(|event| {
            let user_id = event.state_key().unwrap_or(&event.sender);
            let membership = event.membership_change()?;
            Some(KnownRoomMember {
                user_id: user_id.to_owned(),
                display_name: event.membership_display_name().map(str::to_owned),
                membership,
            })
        })
    {
        by_user.insert(member.user_id.clone(), member);
    }
    let mut members = by_user.into_values().collect::<Vec<_>>();
    members.sort_by(|left, right| {
        left.display_name
            .as_deref()
            .unwrap_or(left.user_id.as_str())
            .to_ascii_lowercase()
            .cmp(
                &right
                    .display_name
                    .as_deref()
                    .unwrap_or(right.user_id.as_str())
                    .to_ascii_lowercase(),
            )
    });
    members
}

pub(crate) fn popup_status_lines(app: &App) -> Vec<String> {
    use crate::app::ConnectionState;

    let conn_line = match &app.connection_state {
        ConnectionState::Unknown => "Live WebSocket: not yet connected".to_owned(),
        ConnectionState::Connected => "Live WebSocket: connected".to_owned(),
        ConnectionState::Reconnecting { reason, delay } => {
            format!(
                "Live WebSocket: reconnecting in {}s  ({reason})",
                delay.as_secs()
            )
        }
        ConnectionState::Disconnected(reason) => {
            format!("Live WebSocket: disconnected  ({reason})")
        }
        ConnectionState::ProtocolError(err) => {
            format!("Live WebSocket: protocol error  ({err})")
        }
    };

    let account_filter_line = match app.accounts.selected {
        AccountSelection::All => "Account filter: All Accounts".to_owned(),
        AccountSelection::Account(idx) => {
            let user_id = app
                .accounts
                .accounts
                .get(idx)
                .map(|a| a.user_id.as_str())
                .unwrap_or("?");
            format!("Account filter: {user_id}")
        }
    };

    let auth_line = if app.client.has_bearer_token() {
        "Auth: bearer-token".to_owned()
    } else {
        "Auth: none (insecure, unauthenticated)".to_owned()
    };

    let version = format!("Version: {}", env!("BUILD_INFO"));

    let graphics_line = {
        use ratatui_image::picker::ProtocolType;
        let protocol = match app.picker.protocol_type() {
            ProtocolType::Halfblocks => "halfblocks",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Kitty => "kitty",
            ProtocolType::Iterm2 => "iterm2",
        };
        let FontSize { width, height } = app.picker.font_size();
        format!("Terminal graphics: {protocol}  (cell {width}x{height}px)")
    };

    let mut lines = vec![
        format!("Axon server: {}", app.client.base_url()),
        auth_line,
        version,
        graphics_line,
        conn_line,
        "".to_owned(),
        format!("Rooms loaded: {}", app.rooms.rooms.len()),
        account_filter_line,
        "".to_owned(),
        "Accounts:".to_owned(),
    ];

    if app.accounts.client_visible.is_empty() {
        lines.push("  (none)".to_owned());
    } else {
        for account in &app.accounts.client_visible {
            let state_label = match account.state {
                crate::api::AccountState::Active => "logged in",
                crate::api::AccountState::Deactivated => "logged out",
                crate::api::AccountState::Deleting => "deleting",
            };
            let selected = app.active_account_filter() == Some(account.account_id);
            let marker = if selected { ">" } else { " " };
            let rooms_for_account = app
                .rooms
                .rooms
                .iter()
                .filter(|r| r.account_id == account.account_id)
                .count();
            let duplicate = app
                .accounts
                .client_visible
                .iter()
                .filter(|candidate| candidate.user_id == account.user_id)
                .count()
                > 1;
            let identity = if duplicate {
                format!("{}  [{}]", account.user_id, account.account_id)
            } else {
                account.user_id.clone()
            };
            let device_str = account.device_id.as_deref().unwrap_or("unknown");
            let verified_str = match account.verified {
                Some(true) => "verified",
                Some(false) => "unverified",
                None => "verification unknown",
            };
            lines.push(format!(
                "  {marker} {identity}  ({state_label}, {rooms_for_account} rooms)",
            ));
            lines.push(format!("      device: {device_str}  [{verified_str}]",));
        }
    }

    lines
}

fn room_display_number(visible_position: usize) -> usize {
    visible_position + 1
}

/// Visual layout of the compose buffer for a given inner width: the total number
/// of rows it occupies and the cursor's `(row, col)` within them. Hard line
/// breaks (`\n`, from Shift+Enter) start a new row; long logical lines wrap by
/// character count, matching the input `Paragraph`'s wrapping. A two-column
/// prompt/continuation prefix is accounted for on every logical line.
fn compose_layout(buffer: &str, cursor: usize, inner_width: usize) -> (usize, usize, usize) {
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

// IMPORTANT: update this function whenever a keyboard shortcut is added or removed.
// The shortcuts listed here should be the ones that are discoverable by users through the UI (e.g. not necessarily every single keybinding, but at least all the ones mentioned in the help text or error messages).
pub(crate) fn popup_shortcuts_lines(shortcuts: &Shortcuts) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    enum Row {
        Section(&'static str),
        Dim(&'static str),
        Blank,
        Kv(String, &'static str),
    }

    let rows: Vec<Row> = vec![
        Row::Section("Focus:"),
        Row::Kv(
            shortcuts.focus_next.label(),
            "cycle focus: Input → Accounts → Rooms → Messages",
        ),
        Row::Blank,
        Row::Section("Always active:"),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.next_room.label(),
                shortcuts.previous_room.label()
            ),
            "next / previous room",
        ),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.next_account.label(),
                shortcuts.previous_account.label()
            ),
            "next / previous account (2+ accounts)",
        ),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.message_down.label(),
                shortcuts.message_up.label()
            ),
            "next / previous message",
        ),
        Row::Kv(shortcuts.quit.label(), "quit"),
        Row::Kv(
            shortcuts.toggle_accounts_panel.label(),
            "show/hide Accounts panel",
        ),
        Row::Kv(
            shortcuts.toggle_rooms_panel.label(),
            "show/hide Rooms panel",
        ),
        Row::Kv(shortcuts.refresh.label(), "refresh rooms and redraw"),
        Row::Blank,
        Row::Section("Room list sort & filter:"),
        Row::Kv(
            shortcuts.room_filter_cycle.label(),
            "cycle filter (all / DMs / groups / unread / favorites)",
        ),
        Row::Kv(
            shortcuts.room_sort_cycle.label(),
            "cycle sort (recent / oldest / A–Z / Z–A)",
        ),
        Row::Kv(
            shortcuts.toggle_unread_filter.label(),
            "filter to unread (/filter unread)",
        ),
        Row::Kv(
            shortcuts.room_filter_dms.label(),
            "filter to DMs (/filter dms)",
        ),
        Row::Kv(
            shortcuts.room_filter_groups.label(),
            "filter to group rooms (/filter groups)",
        ),
        Row::Kv(
            shortcuts.room_filter_favorites.label(),
            "filter to favorites (/filter fav)",
        ),
        Row::Kv(
            shortcuts.room_filter_all.label(),
            "clear filter (/filter all)",
        ),
        Row::Kv(
            shortcuts.room_filter_by_name.label(),
            "filter rooms by name (live)",
        ),
        Row::Kv(
            shortcuts.room_sort_recent.label(),
            "sort by recent activity (repeat: oldest)",
        ),
        Row::Kv(shortcuts.room_sort_alpha.label(), "sort A–Z (repeat: Z–A)"),
        Row::Blank,
        Row::Section("Panel resizing:"),
        Row::Kv(
            "Alt-Left / Alt-Right".to_owned(),
            "narrow / widen Accounts or Rooms panel",
        ),
        Row::Kv(
            "Alt-Up / Alt-Down".to_owned(),
            "grow / shrink message entry pane",
        ),
        Row::Blank,
        Row::Section("List navigation:"),
        Row::Kv(shortcuts.find.label(), "start search"),
        Row::Kv("n / N".to_owned(), "next / previous search match (no wrap)"),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.message_page_up.label(),
                shortcuts.message_page_down.label()
            ),
            "page up / down",
        ),
        Row::Kv("Home".to_owned(), "jump to top"),
        Row::Kv("End".to_owned(), "jump to bottom"),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.jump_day_back.label(),
                shortcuts.jump_day_forward.label()
            ),
            "jump to prev / next day with messages",
        ),
        Row::Kv(
            "J".to_owned(),
            "jump to a date in history (/jump also works)",
        ),
        Row::Kv(
            format!("Enter or {}", shortcuts.clear_input.label()),
            "return to Input",
        ),
        Row::Dim("Room list:"),
        Row::Kv(
            shortcuts.pin_room.label(),
            "pin / re-pin selected room to top (/pin)",
        ),
        Row::Kv(shortcuts.unpin_room.label(), "unpin selected room (/unpin)"),
        Row::Blank,
        Row::Section("Message actions (select a message first with Ctrl-J/K):"),
        Row::Kv(shortcuts.edit_message.label(), "edit message"),
        Row::Kv(shortcuts.redact_message.label(), "redact message"),
        Row::Kv(
            shortcuts.react_message.label(),
            "react to message (type emoji name, Tab to cycle)",
        ),
        Row::Kv(shortcuts.unreact_message.label(), "withdraw your reaction"),
        Row::Kv(
            shortcuts.unread_threads.label(),
            "open unread thread picker (/unreadthreads)",
        ),
        Row::Kv(shortcuts.media_preview.label(), "open image preview"),
        Row::Kv(shortcuts.reply.label(), "reply to selected message"),
        Row::Kv(
            shortcuts.thread.label(),
            "open thread, or start one (Esc to exit)",
        ),
        Row::Blank,
        Row::Section("Search results:"),
        Row::Kv("Up / Down".to_owned(), "previous / next result"),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.message_page_up.label(),
                shortcuts.message_page_down.label()
            ),
            "page up / down",
        ),
        Row::Kv("Home / End".to_owned(), "first / last result"),
        Row::Kv(
            shortcuts.search_sort.label(),
            "toggle newest-first / oldest-first sort",
        ),
        Row::Kv(
            shortcuts.search_group.label(),
            "toggle time / room grouping",
        ),
        Row::Kv(shortcuts.search_edit.label(), "edit search"),
        Row::Kv(shortcuts.submit.label(), "jump to selected result"),
        Row::Kv(shortcuts.reply.label(), "reply to selected result"),
        Row::Kv(shortcuts.thread.label(), "thread from selected result"),
        Row::Blank,
        Row::Section("Input:"),
        Row::Kv(
            shortcuts.newline.label(),
            "insert a line break (multi-line message)",
        ),
        Row::Kv(
            "/html <html>".to_owned(),
            "send raw HTML as a formatted message",
        ),
        Row::Kv(
            "/literal <text>".to_owned(),
            "send text as plaintext (skip markdown parsing)",
        ),
        Row::Kv(
            "/rainbow <text>".to_owned(),
            "send text with each character in a rainbow color",
        ),
        Row::Kv(
            "/spoiler [reason |] <text>".to_owned(),
            "send text as a spoiler (dimmed, labeled)",
        ),
        Row::Kv(
            shortcuts.clear_input.label(),
            "clear input / cancel / deselect",
        ),
        Row::Kv(
            format!("{} / Shift-Tab", shortcuts.complete.label()),
            "complete forward / backward",
        ),
        Row::Kv("Ctrl-U".to_owned(), "erase typed text (kill line)"),
        Row::Kv(
            format!(
                "{} / {}",
                shortcuts.edit_previous.label(),
                shortcuts.edit_next.label()
            ),
            "select previous / next message",
        ),
    ];

    let key_col = rows
        .iter()
        .filter_map(|r| {
            if let Row::Kv(k, _) = r {
                Some(k.len())
            } else {
                None
            }
        })
        .max()
        .unwrap_or(16)
        + 2;

    rows.into_iter()
        .map(|row| match row {
            Row::Section(title) => Line::from(Span::styled(title, bold)),
            Row::Dim(title) => Line::from(Span::styled(format!("  {title}"), dim)),
            Row::Blank => Line::from(""),
            Row::Kv(key, desc) => Line::from(vec![
                Span::styled(format!("  {:<width$}", key, width = key_col), bold),
                Span::raw(desc),
            ]),
        })
        .collect()
}

pub(crate) fn entry_status_text(app: &App) -> String {
    app.status.text(app.display.debug)
}

fn search_command_entry_hint(buffer: &str) -> Option<&'static str> {
    let rest = buffer.strip_prefix("/search")?;
    (rest.is_empty() || rest.chars().all(char::is_whitespace))
        .then_some("? for syntax help; type search query now or hit enter for interactive search")
}

fn command_response_prefix_width(app: &App) -> usize {
    let input = if app.show_input_help && app.input.buffer.is_empty() {
        "Type /help or /? for help".to_owned()
    } else {
        mask_login_command(&app.input.buffer)
    };
    4 + Line::from(input).width()
}

fn command_response_line_count(response: &str, width: u16, prefix_width: usize) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return usize::MAX;
    }

    let mut total = 0;
    for (line_index, line) in response.split('\n').enumerate() {
        let mut lines = 1;
        let mut used = if line_index == 0 {
            prefix_width.min(width)
        } else {
            0
        };
        for word in line.split_whitespace() {
            let word_width = Line::from(word).width();
            let separator = usize::from(used > 0);
            if used + separator + word_width <= width {
                used += separator + word_width;
                continue;
            }

            if used > 0 {
                lines += 1;
            }
            lines += word_width.saturating_sub(1) / width;
            used = word_width % width;
            if used == 0 && word_width > 0 {
                used = width;
            }
        }
        total += lines;
    }
    total.max(1)
}

fn wrap_command_response(response: &str, width: u16) -> Vec<String> {
    let width = usize::from(width).max(1);
    let mut wrapped = Vec::new();
    for line in response.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in line.chars() {
            let ch_width = Line::from(ch.to_string()).width();
            if current_width > 0 && current_width + ch_width > width {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
        wrapped.push(current);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn divider_aware_room_scroll(
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

fn command_response_popup_area(response: &str, terminal: Rect) -> Rect {
    const TITLE_WIDTH: u16 = 34;
    const MAX_WIDTH: u16 = 80;

    let available_width = terminal.width.saturating_sub(2).max(1);
    let content_width = response
        .split('\n')
        .map(|line| Line::from(line).width())
        .max()
        .unwrap_or(0)
        .saturating_add(2);
    let width = u16::try_from(content_width)
        .unwrap_or(u16::MAX)
        .clamp(TITLE_WIDTH, MAX_WIDTH)
        .min(available_width);
    let wrapped_height = wrap_command_response(response, width.saturating_sub(2)).len();
    let desired_height = u16::try_from(wrapped_height)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let max_height = terminal.height.saturating_mul(4) / 5;
    let height = desired_height.min(max_height.max(3)).min(terminal.height);

    centered_size(width, height, terminal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{EventDto, RoomDto, SearchResultDto};
    use crate::app::{Status, UnreadThread, UnreadThreadPreview};
    use crate::config::TuiConfig;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

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
    fn shortcuts_popup_lists_configured_navigation_and_actions() {
        let config = TuiConfig::test_default();
        let text = popup_shortcuts_lines(&config.shortcuts)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("F6"));
        assert!(text.contains("Ctrl-J"));
        assert!(text.contains("Ctrl-K"));
        assert!(text.contains("edit message"));
        assert!(text.contains("react to message"));
        assert!(text.contains("withdraw your reaction"));
        assert!(text.contains("open image preview"));
        assert!(text.contains("Up / Down"));
        assert!(text.contains("select previous / next message"));
    }

    #[test]
    fn shortcuts_popup_search_key_follows_configured_find_binding() {
        let config = TuiConfig::test_default();
        let text = popup_shortcuts_lines(&config.shortcuts)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // The search shortcut must be sourced from the configurable `find`
        // binding (default Ctrl-F), never the obsolete hard-coded "/".
        let find_label = config.shortcuts.find.label();
        assert!(
            text.lines()
                .any(|l| l.contains("start search") && l.contains(&find_label)),
            "start-search line should display the configured find binding {find_label:?}"
        );
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
    fn unread_thread_popup_uses_three_preview_lines_per_thread() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let room = RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: "!room:example.com".to_owned(),
            name: Some("Room".to_owned()),
            topic: None,
            avatar_url: None,
            canonical_alias: Some("#room:example.com".to_owned()),
            last_activity_ts: 0,
            last_event_id: None,
        };
        let key = RoomKey::from(&room);
        app.rooms.rooms.push(room);
        app.unread_threads.insert(
            key,
            HashMap::from([(
                "$root:example.com".to_owned(),
                UnreadThread {
                    root_event_id: "$root:example.com".to_owned(),
                    unread_count: 4,
                    latest_event_id: "$reply4:example.com".to_owned(),
                    latest_sender: "@long:example.com".to_owned(),
                    latest_body: "one two three four five six seven eight nine ten".to_owned(),
                    latest_ts: 4,
                    counted: std::collections::HashSet::new(),
                    recent: vec![
                        UnreadThreadPreview {
                            event_id: "$reply4:example.com".to_owned(),
                            sender: "@long:example.com".to_owned(),
                            body: "one two three four five six seven eight nine ten".to_owned(),
                            origin_ts: 4,
                        },
                        UnreadThreadPreview {
                            event_id: "$reply3:example.com".to_owned(),
                            sender: "@short:example.com".to_owned(),
                            body: "older should not render".to_owned(),
                            origin_ts: 3,
                        },
                    ],
                },
            )]),
        );

        let entries = app.unread_thread_entries();
        let (lines, ranges) = popup_unread_thread_lines(&app, &entries, 28);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert_eq!(ranges, vec![0..4]);
        assert_eq!(lines.len(), 4);
        assert!(text.contains("@long:example.com"));
        assert!(!text.contains("@short:example.com"));
    }

    #[test]
    fn unread_thread_popup_puts_newest_preview_at_bottom() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let room = RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@alice:example.com".to_owned()),
            room_id: "!room:example.com".to_owned(),
            name: Some("Room".to_owned()),
            topic: None,
            avatar_url: None,
            canonical_alias: Some("#room:example.com".to_owned()),
            last_activity_ts: 0,
            last_event_id: None,
        };
        let key = RoomKey::from(&room);
        app.rooms.rooms.push(room);
        app.unread_threads.insert(
            key,
            HashMap::from([(
                "$root:example.com".to_owned(),
                UnreadThread {
                    root_event_id: "$root:example.com".to_owned(),
                    unread_count: 3,
                    latest_event_id: "$reply3:example.com".to_owned(),
                    latest_sender: "@newest:example.com".to_owned(),
                    latest_body: "newest".to_owned(),
                    latest_ts: 3,
                    counted: std::collections::HashSet::new(),
                    recent: vec![
                        UnreadThreadPreview {
                            event_id: "$reply3:example.com".to_owned(),
                            sender: "@newest:example.com".to_owned(),
                            body: "newest".to_owned(),
                            origin_ts: 3,
                        },
                        UnreadThreadPreview {
                            event_id: "$reply2:example.com".to_owned(),
                            sender: "@middle:example.com".to_owned(),
                            body: "middle".to_owned(),
                            origin_ts: 2,
                        },
                        UnreadThreadPreview {
                            event_id: "$reply1:example.com".to_owned(),
                            sender: "@oldest:example.com".to_owned(),
                            body: "oldest".to_owned(),
                            origin_ts: 1,
                        },
                    ],
                },
            )]),
        );

        let entries = app.unread_thread_entries();
        let (lines, ranges) = popup_unread_thread_lines(&app, &entries, 80);
        let preview_lines = lines
            .iter()
            .skip(ranges[0].start + 1)
            .take(3)
            .map(line_text)
            .collect::<Vec<_>>();

        assert!(preview_lines[0].contains("@oldest:example.com"));
        assert!(preview_lines[1].contains("@middle:example.com"));
        assert!(preview_lines[2].contains("@newest:example.com"));
    }

    #[test]
    fn modal_popups_block_thumbnail_refresh() {
        let screen = Rect::new(0, 0, 120, 40);
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );

        // No modal open: refreshes are free to render thumbnails.
        assert!(blocking_popup_area(&app, screen).is_none());

        // Every modal popup occupies a centered region; a thumbnail in the
        // message pane that overlaps it must be suppressed so a background
        // refresh never paints over the modal.
        for kind in [PopupKind::Help, PopupKind::Shortcuts, PopupKind::Status] {
            app.mode = Mode::Popup(kind);
            let area = blocking_popup_area(&app, screen).expect("modal area");
            let overlapping = Rect::new(area.x, area.y, 4, 2);
            assert!(
                thumbnail_overlaps_blocking_popup(overlapping, Some(area)),
                "{kind:?} modal should suppress overlapping thumbnails"
            );
        }
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
    fn thumbnail_rendering_is_suppressed_under_media_preview_area() {
        let popup = Rect::new(20, 5, 40, 12);

        assert!(thumbnail_overlaps_blocking_popup(
            Rect::new(10, 8, 20, 6),
            Some(popup)
        ));
        assert!(!thumbnail_overlaps_blocking_popup(
            Rect::new(0, 0, 10, 4),
            Some(popup)
        ));
        assert!(!thumbnail_overlaps_blocking_popup(
            Rect::new(10, 8, 20, 6),
            None
        ));
    }

    #[test]
    fn preview_reserves_height_for_caption() {
        let bounds = Rect::new(0, 0, 80, 20);
        let (image, caption_h) = fit_preview_caption(Size::new(80, 20), 3, bounds);

        assert_eq!(image, Size::new(80, 17));
        assert_eq!(caption_h, 3);
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

    #[test]
    fn room_numbers_use_absolute_visible_positions() {
        assert_eq!(room_display_number(0), 1);
        assert_eq!(room_display_number(5), 6);
    }

    #[test]
    fn account_localpart_tag_omits_matrix_sigil() {
        assert_eq!(account_localpart("@alice:example.com"), Some("alice"));
    }

    #[test]
    fn search_command_entry_hint_only_shows_before_arguments() {
        assert_eq!(
            search_command_entry_hint("/search"),
            Some("? for syntax help; type search query now or hit enter for interactive search")
        );
        assert_eq!(
            search_command_entry_hint("/search "),
            Some("? for syntax help; type search query now or hit enter for interactive search")
        );
        assert_eq!(search_command_entry_hint("/search ?"), None);
        assert_eq!(search_command_entry_hint("/search help"), None);
        assert_eq!(search_command_entry_hint("/status"), None);
    }

    #[test]
    fn search_results_lines_use_available_height() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let account_id = Uuid::nil();
        let results = (0..20)
            .map(|i| SearchResultDto {
                event: EventDto {
                    account_id,
                    event_id: format!("${i}:example.org"),
                    room_id: "!room:example.org".to_owned(),
                    sender: "@alice:example.org".to_owned(),
                    state_key: None,
                    arrival_order: i,
                    origin_ts: i,
                    event_type: "m.room.message".to_owned(),
                    content: None,
                    body: Some(format!("result {i}")),
                    relates_to: None,
                    redacted: false,
                    redaction_event_id: None,
                    reactions: None,
                    sender_trust: None,
                },
                score: 1.0,
            })
            .collect::<Vec<_>>();
        app.search_results = Some(crate::search::SearchResultsState {
            request: crate::search::SearchRequest {
                q: "result".to_owned(),
                account_id: None,
                room_id: None,
                sender: None,
                from: None,
                to: None,
                limit: crate::search::DEFAULT_SEARCH_LIMIT,
                cursor: None,
            },
            edit_form: crate::search::SearchFormState::from_parsed(
                &crate::search::parse_search_terms("result").unwrap(),
            ),
            results,
            total: 20,
            next_cursor: None,
            selected: 0,
            loading: false,
            sort_order: crate::search::SearchSortOrder::NewestFirst,
            grouping: crate::search::SearchGrouping::None,
            context_cache: Default::default(),
        });

        let rendered = search_results_lines(&app, 100, 30);
        let rendered_result_headers = rendered
            .iter()
            .map(line_text)
            .filter(|line| line.contains(". ") && line.contains("!room:example.org"))
            .count();

        assert!(
            rendered_result_headers > 10,
            "a tall results pane should render beyond the old fixed 10-result cap"
        );
    }

    #[test]
    fn search_results_lines_scroll_with_selection() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let account_id = Uuid::nil();
        let results = (0..20)
            .map(|i| SearchResultDto {
                event: EventDto {
                    account_id,
                    event_id: format!("${i}:example.org"),
                    room_id: "!room:example.org".to_owned(),
                    sender: "@alice:example.org".to_owned(),
                    state_key: None,
                    arrival_order: i,
                    origin_ts: i,
                    event_type: "m.room.message".to_owned(),
                    content: None,
                    body: Some(format!("result {i}")),
                    relates_to: None,
                    redacted: false,
                    redaction_event_id: None,
                    reactions: None,
                    sender_trust: None,
                },
                score: 1.0,
            })
            .collect::<Vec<_>>();
        app.search_results = Some(crate::search::SearchResultsState {
            request: crate::search::SearchRequest {
                q: "result".to_owned(),
                account_id: None,
                room_id: None,
                sender: None,
                from: None,
                to: None,
                limit: crate::search::DEFAULT_SEARCH_LIMIT,
                cursor: None,
            },
            edit_form: crate::search::SearchFormState::from_parsed(
                &crate::search::parse_search_terms("result").unwrap(),
            ),
            results,
            total: 20,
            next_cursor: None,
            selected: 2,
            loading: false,
            sort_order: crate::search::SearchSortOrder::OldestFirst,
            grouping: crate::search::SearchGrouping::None,
            context_cache: Default::default(),
        });

        let top_text = search_results_lines(&app, 100, 10)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        app.search_results.as_mut().unwrap().selected = 12;
        let scrolled_text = search_results_lines(&app, 100, 10)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(top_text.contains("result 0"));
        assert!(!top_text.contains("result 12"));
        assert!(!scrolled_text.contains("result 0"));
        assert!(scrolled_text.contains(">  13."));
    }

    #[test]
    fn truncate_chars_respects_cjk_display_width() {
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("漢字abc", 7), "漢字abc");
        assert_eq!(truncate_chars("漢字abc", 6), "漢...");
    }

    #[test]
    fn overflowing_command_response_opens_popup() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let response =
            "This command response is long enough to wrap beyond the one-line entry box.";
        app.show_input_help = false;
        app.status = Status::Info(response.to_owned());
        app.pending_command_response = Some(response.to_owned());
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw succeeds");

        assert_eq!(app.mode, Mode::Popup(PopupKind::CommandResponse));
        assert_eq!(app.pending_command_response.as_deref(), Some(response));
        let buffer = terminal.backend().buffer();
        let input_rows = buffer.area.height.saturating_sub(3)..buffer.area.height;
        let input_text = input_rows
            .flat_map(|y| {
                (0..buffer.area.width)
                    .filter_map(move |x| buffer.cell((x, y)).map(|c| c.symbol().to_owned()))
            })
            .collect::<String>();
        assert!(
            !input_text.contains("This command response"),
            "overflowing command response should render in the popup, not the input bar"
        );
    }

    #[test]
    fn inbound_typing_overlay_renders_on_the_message_pane() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.rooms.rooms = vec![crate::api::RoomDto {
            account_id: uuid::Uuid::nil(),
            account_user_id: Some("@me:hs".to_owned()),
            room_id: "!r:hs".to_owned(),
            name: Some("Room".to_owned()),
            topic: None,
            avatar_url: None,
            canonical_alias: None,
            last_activity_ts: 0,
            last_event_id: None,
        }];
        app.rooms.selected = Some(0);
        app.seed_own_senders_from_rooms();
        app.handle_ephemeral_frame(
            uuid::Uuid::nil(),
            crate::api::EphemeralPassthroughDto {
                room_id: Some("!r:hs".to_owned()),
                event_type: "m.typing".to_owned(),
                content: serde_json::json!({ "user_ids": ["@bob:hs"] }),
            },
            std::time::Instant::now(),
        );

        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw succeeds");

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .flat_map(|y| {
                (0..buffer.area.width)
                    .filter_map(move |x| buffer.cell((x, y)).map(|c| c.symbol().to_owned()))
            })
            .collect::<String>();
        assert!(
            rendered.contains("@bob:hs is typing"),
            "typing overlay should be visible on the message pane"
        );
    }

    #[test]
    fn multiline_compose_buffer_renders_on_separate_rows() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.show_input_help = false;
        app.mode = Mode::Compose;
        app.input.buffer = "first\nsecond".to_owned();
        app.input.cursor = app.input.buffer.len();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw succeeds");

        // Collect each rendered row as a string and locate the two hard lines.
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect();
        let first_row = rows.iter().position(|r| r.contains("first"));
        let second_row = rows.iter().position(|r| r.contains("second"));
        assert!(first_row.is_some(), "first hard line should render");
        assert!(second_row.is_some(), "second hard line should render");
        assert!(
            second_row > first_row,
            "the second hard line renders below the first"
        );
    }

    #[test]
    fn thumbnail_overlap_uses_actual_image_rect_not_full_reserved_box() {
        // A popup sitting center-right of the message pane.
        let modal = Rect::new(60, 5, 40, 20);
        // The thumbnail reserves the full body width (x=3, width 100), but the
        // actual aspect-fit image is only 12 cells wide, anchored top-left.
        let reserved = Rect::new(3, 8, 100, 8);
        let drawn = image_draw_rect(reserved, Size::new(12, 8));
        assert_eq!(drawn, Rect::new(3, 8, 12, 8));
        // The reserved box spuriously overlaps the modal; the real pixels do not.
        assert!(thumbnail_overlaps_blocking_popup(reserved, Some(modal)));
        assert!(
            !thumbnail_overlaps_blocking_popup(drawn, Some(modal)),
            "a thumbnail whose pixels are clear of the popup must still render"
        );
    }

    #[test]
    fn image_draw_rect_clamps_to_reserved_box() {
        // An image larger than its reserved box is clamped (never exceeds it).
        let drawn = image_draw_rect(Rect::new(5, 5, 10, 4), Size::new(40, 40));
        assert_eq!(drawn, Rect::new(5, 5, 10, 4));
    }

    #[test]
    fn preview_modal_area_shrinks_to_image_so_outside_thumbnails_survive() {
        use crate::app::MediaKey;
        use std::sync::Arc;

        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(), // font size 10x20
        );
        // A small image (100x60 px -> ~10x3 cells) yields a small modal.
        let media = MediaKey::new(Uuid::from_u128(1), "mxc://example.com/small".to_owned());
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(100, 60));
        app.image_cache
            .insert(media.clone(), ImageState::Ready(Arc::new(img)));

        let screen = Rect::new(0, 0, 200, 60);
        let max_area = centered_rect(PREVIEW_MAX_PCT, PREVIEW_MAX_PCT, screen);
        let (area, _, _) = media_preview_layout(&app, screen, None, &media);

        // The modal hugs the image rather than filling the 88% max.
        assert!(
            area.width < max_area.width && area.height < max_area.height,
            "modal {area:?} should be smaller than max {max_area:?}"
        );

        // A timeline thumbnail to the left of the small centered modal is NOT
        // suppressed — the regression was suppressing it against the 88% max.
        let left_thumb = Rect::new(3, 5, 20, 3);
        assert!(
            !thumbnail_overlaps_blocking_popup(left_thumb, Some(area)),
            "thumbnail outside the actual modal must render"
        );
        assert!(
            thumbnail_overlaps_blocking_popup(left_thumb, Some(max_area)),
            "regression guard: it would have been suppressed against the 88% max"
        );
    }

    #[test]
    fn fitting_command_response_stays_in_entry_box() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.show_input_help = false;
        app.status = Status::Info("done".to_owned());
        app.pending_command_response = Some("done".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw succeeds");

        assert_eq!(app.mode, Mode::Compose);
        assert!(app.pending_command_response.is_none());
    }

    #[test]
    fn restored_command_input_reduces_available_response_width() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        app.show_input_help = false;
        app.input.buffer = "/recover alice".to_owned();
        app.input.cursor = app.input.buffer.len();
        app.status = Status::Info("recovery failed".to_owned());
        app.pending_command_response = Some("recovery failed".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).expect("terminal");

        terminal
            .draw(|frame| draw(frame, &mut app))
            .expect("draw succeeds");

        assert_eq!(app.mode, Mode::Popup(PopupKind::CommandResponse));
    }

    #[test]
    fn command_response_wrap_count_honors_words_and_newlines() {
        assert_eq!(command_response_line_count("done", 20, 4), 1);
        assert_eq!(command_response_line_count("12345 12345 12345", 10, 4), 3);
        assert_eq!(command_response_line_count("first\nsecond", 20, 4), 2);
    }

    #[test]
    fn command_response_popup_wraps_long_lines_for_scrolling() {
        let lines = wrap_command_response(&"x".repeat(25), 10);

        assert_eq!(lines, vec!["x".repeat(10), "x".repeat(10), "x".repeat(5)]);
    }

    #[test]
    fn command_response_popup_height_fits_short_content() {
        let area = command_response_popup_area("recovery failed", Rect::new(0, 0, 120, 40));

        assert_eq!(area.height, 3);
        assert_eq!(area.width, 34);
        assert_eq!(area.x, 43);
        assert_eq!(area.y, 18);
    }

    #[test]
    fn command_response_popup_is_clamped_for_small_terminals() {
        let area = command_response_popup_area(&"x".repeat(200), Rect::new(0, 0, 30, 10));

        assert_eq!(area.width, 28);
        assert_eq!(area.height, 8);
        assert_eq!(area.x, 1);
        assert_eq!(area.y, 1);
    }

    #[test]
    fn masks_inline_login_password_without_hiding_username() {
        assert_eq!(
            mask_login_command("/login @alice:example.com secret phrase"),
            "/login @alice:example.com •••••• ••••••"
        );
        assert_eq!(
            mask_login_command("  /login @alice:example.com secret"),
            "  /login @alice:example.com ••••••"
        );
        assert_eq!(
            mask_login_command("/login @alice:example.com"),
            "/login @alice:example.com"
        );
        assert_eq!(mask_login_command("/logout alice"), "/logout alice");
    }

    #[test]
    fn room_info_popup_lists_summary_and_known_members() {
        let mut app = App::new(
            crate::api::AxonClient::new("http://127.0.0.1:8080".to_owned(), None),
            None,
            TuiConfig::test_default(),
            ratatui_image::picker::Picker::halfblocks(),
        );
        let room = RoomDto {
            account_id: Uuid::nil(),
            account_user_id: Some("@me:example.com".to_owned()),
            room_id: "!room:example.com".to_owned(),
            name: Some("Ops".to_owned()),
            topic: Some("Daily operations".to_owned()),
            avatar_url: Some("mxc://example/avatar".to_owned()),
            canonical_alias: Some("#ops:example.com".to_owned()),
            last_activity_ts: 0,
            last_event_id: Some("$last:example.com".to_owned()),
        };
        app.rooms.rooms = vec![room.clone()];
        app.rooms.selected = Some(0);
        app.messages.events.insert(
            crate::app::RoomKey::from(&room),
            vec![EventDto {
                account_id: Uuid::nil(),
                event_id: "$member:example.com".to_owned(),
                room_id: "!room:example.com".to_owned(),
                sender: "@alice:example.com".to_owned(),
                state_key: Some("@alice:example.com".to_owned()),
                arrival_order: 0,
                origin_ts: 0,
                event_type: "m.room.member".to_owned(),
                content: Some(serde_json::json!({
                    "membership": "join",
                    "displayname": "Alice"
                })),
                body: None,
                relates_to: None,
                redacted: false,
                redaction_event_id: None,
                reactions: None,
                sender_trust: None,
            }],
        );

        let text = popup_room_info_lines(&app).join("\n");

        assert!(text.contains("Name: Ops"));
        assert!(text.contains("Matrix ID: !room:example.com"));
        assert!(text.contains("Aliases: #ops:example.com"));
        assert!(text.contains("Topic: Daily operations"));
        assert!(text.contains("Alice  @alice:example.com  (join)"));
        assert!(text.contains("Encryption: unavailable"));
    }
}
