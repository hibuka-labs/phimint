//! Mouse event handling for the TUI application.
//!
//! Handles mouse clicks, drags, and the right-click copy menu,
//! operating on `App`'s selection state and viewport.

use crossterm::event::{MouseButton, MouseEventKind};

use crate::ui::app::{Action, App, ContextMenu};
use phi_tui::selection::MenuClick;

impl App {
    /// Handle a mouse event at `(x, y)` (terminal cells) against a terminal of
    /// `(area_w, area_h)` cells: left-press anchors a selection, left-drag
    /// extends it, right-press opens the copy menu. While the copy menu is
    /// open, a click inside its popup activates that item — "拷贝" copies the
    /// selection, "取消" closes — and returns `Action::CopySelection` for the
    /// caller to run (mirrors the keyboard Enter path).
    pub(crate) fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        x: u16,
        y: u16,
        area_w: u16,
        area_h: u16,
    ) -> Option<Action> {
        use MouseEventKind::*;

        // Menu is open: a click (either button) inside the drawn popup activates
        // the item under the cursor instead of falling through to selection.
        if let Some(click) = self.selection_state.menu_click(kind, x, y, area_w, area_h) {
            return match click {
                MenuClick::Copy => Some(Action::CopySelection),
                MenuClick::Dismiss => None,
            };
        }

        match kind {
            Down(MouseButton::Left) => {
                let idx = self.line_index_at(x, y);
                tracing::debug!(x, y, selected_idx = ?idx, "mouse left down");
                self.selection_state.anchor(idx);
                None
            }
            Drag(MouseButton::Left) => {
                let idx = self.line_index_at(x, y);
                self.selection_state.extend(idx);
                None
            }
            Up(MouseButton::Left) => None,
            Down(MouseButton::Right) => {
                self.selection_state.open_menu(x, y);
                None
            }
            _ => None,
        }
    }

    /// Map a screen cell `(x, y)` to an `output` line index, or `None` when
    /// outside the output pane or over a still-streaming (uncommitted) line.
    fn line_index_at(&self, x: u16, y: u16) -> Option<usize> {
        let (ax, ay, aw, ah) = self.output_area?;
        if x < ax || x >= ax.saturating_add(aw) || y < ay || y >= ay.saturating_add(ah) {
            return None;
        }
        let row = (y - ay) as usize;
        let committed = self.transcript.len();

        // Use the visual map when available (after render), otherwise
        // fall back to direct output indices (for tests or before first render).
        if self.visual_map.is_empty() {
            // Fallback: no markdown expansion, output lines map 1:1 to visual lines.
            let tail = self.streaming_tail_lines().map(|(l, _)| l.len()).unwrap_or(0);
            let window = self.viewport.window_range(committed + tail, ah as usize);
            let idx = window.start + row;
            tracing::debug!(
                x, y, row, committed, tail,
                window_start = window.start, window_end = window.end, idx,
                "line_index_at (fallback, no mapping)"
            );
            if idx < window.end && idx < committed {
                return Some(idx);
            }
            return None;
        }

        let total = self.viewport.rendered_total;
        let window = self.viewport.window_range(total, ah as usize);
        let visual_idx = window.start + row;
        let out_idx = self.visual_map.output_at(visual_idx);
        tracing::debug!(
            x, y, row, total, committed,
            window_start = window.start, window_end = window.end,
            visual_idx, out_idx,
            mapping_len = self.visual_map.len(),
            "line_index_at"
        );
        if visual_idx < window.end
            && let Some(mapped) = self.visual_map.output_at(visual_idx)
            && mapped < committed
        {
            return Some(visual_idx); // return visual index, not output index
        }
        None
    }

    /// True when visual line `i` falls inside the active selection.
    pub fn is_selected(&self, i: usize) -> bool {
        let total = if self.visual_map.is_empty() {
            self.transcript.len()
        } else {
            self.viewport.rendered_total
        };
        self.selection_state.is_selected(i, total)
    }

    /// The selected lines joined as plain text (what-you-see-is-what-you-copy).
    /// Uses the visual map's row texts so the copy matches exactly what the
    /// user selected visually, not the full raw `output` block.
    pub fn selection_text(&self) -> String {
        let fallback: Vec<&str> = self.transcript.output.iter().map(|l| l.text.as_str()).collect();
        self.selection_state
            .text(&fallback, self.visual_map.texts())
    }

    /// Clear the active transcript selection.
    pub fn clear_selection(&mut self) {
        self.selection_state.clear();
    }

    /// The right-click menu, if open.
    pub fn context_menu(&self) -> Option<&ContextMenu> {
        self.selection_state.context_menu()
    }
}
