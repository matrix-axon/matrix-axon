//! Panel geometry: which side panels are showing, how wide they are, and how
//! many rows the compose box and message viewport get.
//!
//! The knobs the key handlers turn and the renderer reads, gathered in one
//! place so a change to the panel layout has a single home. This module holds
//! the [`App`] state and the bounds each knob is clamped to; the cell
//! arithmetic that turns them into rects is [`crate::geometry`].

use super::App;

impl App {
    pub(crate) fn accounts_panel_visible(&self) -> bool {
        !self.accounts_panel_hidden && self.accounts.accounts.len() >= 2
    }

    pub(crate) fn rooms_panel_visible(&self) -> bool {
        !self.rooms_panel_hidden
    }

    pub(crate) fn adjust_accounts_width(&mut self, delta: i16) {
        const MIN: u16 = 10;
        const MAX: u16 = 60;
        self.display.accounts_panel_width =
            (self.display.accounts_panel_width as i16 + delta).clamp(MIN as i16, MAX as i16) as u16;
    }

    pub(crate) fn adjust_rooms_width(&mut self, delta: i16) {
        self.display.rooms_panel_width_adj = self
            .display
            .rooms_panel_width_adj
            .saturating_add(delta)
            .clamp(-50, 50);
    }

    pub(crate) fn adjust_input_lines(&mut self, delta: i16) {
        self.display.input_lines = (self.display.input_lines as i16 + delta).clamp(1, 10) as u16;
    }

    pub(crate) fn set_message_viewport(&mut self, page_size: usize, width: usize) {
        self.messages.page_size = page_size.max(1);
        self.messages.width = width.max(1);
    }
}
