//! The view model one frame is painted from.
//!
//! [`ui::prepare`](crate::ui::prepare) fills this in the update step, before
//! [`ui::draw`](crate::ui::draw) paints; `draw` then reads it and recomputes
//! none of it. Splitting the two is what keeps the renderer from deciding
//! model state that key handling also reads — viewport sizes, scroll offsets,
//! popup selection (#70). Before the split, a `PageDown` was paged by a
//! `page_size` the *previous* repaint had measured, and nothing said so.
//!
//! Everything here is rebuilt from scratch once per frame, so nothing in it
//! needs invalidating; it is a cache of this frame's answers, not of state.
//! The durable counterparts — `rooms.scroll`, `popup_scroll`, the message
//! layout — live on [`App`](super::App) proper, because a key handler moves
//! them and they have to survive to the next frame.

use ratatui::layout::Rect;
use ratatui::text::Line;

use super::AccountSelection;

/// Where every pane landed on this frame's screen.
///
/// Computed once by [`ui::pane_areas`](crate::ui::pane_areas) and stored here,
/// rather than recomputed by each half — the renderer and the update step
/// painting to two different sets of rects is exactly the failure this split
/// exists to prevent (#70), and one stored answer makes it unrepresentable
/// instead of merely unlikely.
#[derive(Default, Clone, Copy)]
pub(crate) struct PaneAreas {
    /// Everything above the entry box. The panes are carved out of this, and a
    /// full-screen search result list replaces it wholesale.
    pub(crate) body: Rect,
    pub(crate) accounts: Option<Rect>,
    pub(crate) rooms: Option<Rect>,
    pub(crate) messages: Rect,
    pub(crate) input: Rect,
    /// Content rows the entry box ended up with, after any `max_input_lines`
    /// growth. Excludes its two borders.
    pub(crate) input_lines: u16,
    /// The room pane is wide enough to give each room a single row.
    pub(crate) rooms_wide: bool,
}

#[derive(Default)]
pub(crate) struct FrameState {
    /// This frame's pane geometry, measured once.
    pub(crate) areas: PaneAreas,
    /// Accounts-pane rows surviving the active search filter, in display
    /// order. Empty while the pane is hidden.
    pub(crate) accounts: Vec<(String, AccountSelection)>,
    /// Indices into `RoomsState::rooms` the room pane shows, in display order.
    /// Empty while the pane is hidden.
    pub(crate) rooms: Vec<usize>,
    /// Length of the leading run of pinned rooms in [`Self::rooms`] — where
    /// the pinned/unpinned divider goes (ADR 0038). Zero when none are pinned.
    pub(crate) pinned_rooms: usize,
    /// The open popup's resolved body, or `None` when no popup is open.
    ///
    /// The media preview is the one popup that is never resolved here: it
    /// paints pixels rather than lines, and which pixels depends on a decode
    /// that may still be in flight, so `draw` owns it end to end.
    pub(crate) popup: Option<PopupView>,
}

/// A popup's fully resolved body: the exact lines to paint and the rect to
/// paint them in.
///
/// The scroll offset is deliberately *not* here — it lives on
/// `App::popup_scroll`, because the scroll keys move it and it has to outlive
/// the frame. `prepare` clamps it against [`Self::lines`] before `draw` runs,
/// which is the only reason the unclamped value a key handler can leave behind
/// is never painted.
pub(crate) struct PopupView {
    pub(crate) title: &'static str,
    pub(crate) area: Rect,
    pub(crate) lines: Vec<Line<'static>>,
    /// Rows of [`Self::lines`] visible at once: `area` less its borders.
    pub(crate) page_size: usize,
}
