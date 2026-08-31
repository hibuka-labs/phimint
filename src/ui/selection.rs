//! 转录本鼠标选择 + 右键复制菜单状态。
//!
//! 从 `App` god-object 抽出（此前是 `selection`/`context_menu` 字段 +
//! `handle_mouse` 里的选择分支、`selection_range`/`is_selected`/
//! `selection_text`/`clear_selection`）。屏幕坐标 → 行号的命中测试依赖渲染
//! 产物（`visual_to_output` 等），留在 App；这里只管理「选了什么 / 菜单开没
//! 开 / 范围与文本」。

use crossterm::event::{MouseButton, MouseEventKind};

use crate::ui::app::OutputLine;

/// A line-range selection into the transcript (inclusive). `anchor` is the drag
/// start and `head` the current line; they may be in either order, so
/// [`Selection::bounds`] normalizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
}

impl Selection {
    /// Normalized inclusive `(lo, hi)` bounds, `lo <= hi`.
    fn bounds(self) -> (usize, usize) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

/// Right-click copy menu: anchor cell (top-left) and highlighted item index
/// (0 = copy, 1 = cancel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMenu {
    pub x: u16,
    pub y: u16,
    pub selected: usize,
}

/// Context-menu popup geometry, shared between rendering and mouse hit-testing
/// so a click lands exactly on the drawn popup.
pub const CONTEXT_MENU_W: u16 = 12;
pub const CONTEXT_MENU_H: u16 = 4; // border + two items

/// Clamp the menu's top-left so the fixed-size popup stays fully on screen.
pub fn context_menu_pos(x: u16, y: u16, area_w: u16, area_h: u16) -> (u16, u16) {
    (
        x.min(area_w.saturating_sub(CONTEXT_MENU_W)),
        y.min(area_h.saturating_sub(CONTEXT_MENU_H)),
    )
}

/// 右键菜单开着时的一次点击结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuClick {
    /// 点了「拷贝」。
    Copy,
    /// 点了「取消」（或菜单外区域，交给普通选择逻辑）。
    Dismiss,
}

/// Mouse selection + copy-menu state machine.
#[derive(Debug, Default)]
pub struct SelectionState {
    pub selection: Option<Selection>,
    pub context_menu: Option<ContextMenu>,
}

impl SelectionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn context_menu(&self) -> Option<&ContextMenu> {
        self.context_menu.as_ref()
    }

    pub fn clear(&mut self) {
        self.selection = None;
    }

    pub fn menu_is_open(&self) -> bool {
        self.context_menu.is_some()
    }

    pub fn menu_selected(&self) -> usize {
        self.context_menu.map_or(0, |m| m.selected)
    }

    /// Move the menu highlight by `delta` (±1); item 1 is the last item.
    pub fn menu_move(&mut self, delta: i32) {
        let Some(m) = self.context_menu.as_mut() else {
            return;
        };
        if delta < 0 {
            m.selected = m.selected.saturating_sub(1);
        } else {
            m.selected = (m.selected + 1).min(1);
        }
    }

    /// Left-press: drop any open menu and anchor a new selection at `idx`
    /// (`None` = the press missed the transcript — clears the selection).
    pub fn anchor(&mut self, idx: Option<usize>) {
        self.context_menu = None;
        self.selection = idx.map(|idx| Selection { anchor: idx, head: idx });
    }

    /// Left-drag: move the selection head to `idx` (no-op when no selection).
    pub fn extend(&mut self, idx: Option<usize>) {
        if let Some(idx) = idx {
            if let Some(sel) = self.selection.as_mut() {
                sel.head = idx;
            }
        }
    }

    /// Right-press: open the copy menu when a selection is active.
    pub fn open_menu(&mut self, x: u16, y: u16) {
        if self.selection.is_some() {
            self.context_menu = Some(ContextMenu { x, y, selected: 0 });
        }
    }

    /// While the menu is open, a click (either button) inside the drawn popup
    /// activates the item under the cursor. Returns `None` when the click fell
    /// outside the popup (the caller falls through to normal selection).
    pub fn menu_click(
        &mut self,
        kind: MouseEventKind,
        x: u16,
        y: u16,
        area_w: u16,
        area_h: u16,
    ) -> Option<MenuClick> {
        let menu = self.context_menu?;
        let (mx, my) = context_menu_pos(menu.x, menu.y, area_w, area_h);
        if x >= mx
            && x < mx + CONTEXT_MENU_W
            && y >= my
            && y < my + CONTEXT_MENU_H
            && matches!(
                kind,
                MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Down(MouseButton::Right)
            )
        {
            let clicked_copy = y.saturating_sub(my + 1) == 0;
            self.context_menu = None;
            return Some(if clicked_copy {
                MenuClick::Copy
            } else {
                MenuClick::Dismiss
            });
        }
        None
    }

    /// The active selection as a normalized, `total`-clamped inclusive range,
    /// or `None` when empty/stale. Indices are into the visual line array
    /// (post-markdown-expansion), not the raw transcript.
    pub fn range(&self, total: usize) -> Option<(usize, usize)> {
        let sel = self.selection?;
        let (lo, hi) = sel.bounds();
        if total == 0 || lo >= total {
            return None;
        }
        Some((lo, hi.min(total - 1)))
    }

    /// True when visual line `i` falls inside the active selection.
    pub fn is_selected(&self, i: usize, total: usize) -> bool {
        self.range(total)
            .map_or(false, |(lo, hi)| lo <= i && i <= hi)
    }

    /// The selected lines joined as plain text (what-you-see-is-what-you-copy).
    /// Uses `visual_lines_text` so the copy matches exactly what the user
    /// selected visually; falls back to the raw `output` lines (tests, before
    /// the first render).
    pub fn text(&self, output: &[OutputLine], visual_lines_text: &[String]) -> String {
        let total = if visual_lines_text.is_empty() {
            output.len()
        } else {
            visual_lines_text.len()
        };
        let Some((lo, hi)) = self.range(total) else {
            return String::new();
        };
        if visual_lines_text.is_empty() {
            return output[lo..=hi.min(output.len().saturating_sub(1))]
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
        }
        visual_lines_text[lo..=hi.min(visual_lines_text.len().saturating_sub(1))]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::app::LineKind;

    fn lines(n: usize) -> Vec<OutputLine> {
        (0..n)
            .map(|i| OutputLine {
                spans: None,
                original: None,
                detail: None,
                text: format!("line {i}"),
                kind: LineKind::Normal,
            })
            .collect()
    }

    #[test]
    fn drag_selects_and_text_joins() {
        let mut s = SelectionState::new();
        s.anchor(Some(2));
        s.extend(Some(5));
        assert_eq!(s.selection, Some(Selection { anchor: 2, head: 5 }));
        assert_eq!(s.text(&lines(10), &[]), "line 2\nline 3\nline 4\nline 5");
    }

    #[test]
    fn drag_up_normalizes_bounds() {
        let mut s = SelectionState::new();
        s.anchor(Some(5));
        s.extend(Some(2));
        assert!(s.is_selected(3, 10));
        assert_eq!(s.text(&lines(10), &[]), "line 2\nline 3\nline 4\nline 5");
    }

    #[test]
    fn anchor_outside_transcript_clears() {
        let mut s = SelectionState::new();
        s.anchor(Some(0));
        assert!(s.selection.is_some());
        s.anchor(None); // click below the pane
        assert!(s.selection.is_none());
    }

    #[test]
    fn text_clamps_stale_indices() {
        let mut s = SelectionState::new();
        s.anchor(Some(0));
        s.extend(Some(5));
        assert_eq!(s.text(&lines(1), &[]), "line 0");
    }

    #[test]
    fn open_menu_requires_selection() {
        let mut s = SelectionState::new();
        s.open_menu(3, 4);
        assert!(!s.menu_is_open());
        s.anchor(Some(0));
        s.open_menu(3, 4);
        assert_eq!(s.context_menu(), Some(&ContextMenu { x: 3, y: 4, selected: 0 }));
    }

    #[test]
    fn menu_move_and_close() {
        let mut s = SelectionState::new();
        s.anchor(Some(0));
        s.open_menu(0, 0);
        s.menu_move(1);
        assert_eq!(s.menu_selected(), 1);
        s.menu_move(1);
        assert_eq!(s.menu_selected(), 1, "clamped at last item");
        s.menu_move(-1);
        assert_eq!(s.menu_selected(), 0);
        s.context_menu = None;
        assert!(!s.menu_is_open());
    }

    #[test]
    fn menu_click_hit_test() {
        use crossterm::event::{MouseButton, MouseEventKind};
        let mut s = SelectionState::new();
        s.anchor(Some(0));
        s.open_menu(0, 0);
        // Click the "拷贝" row (row y+1) → Copy.
        assert_eq!(
            s.menu_click(MouseEventKind::Down(MouseButton::Left), 1, 1, 20, 20),
            Some(MenuClick::Copy)
        );
        assert!(!s.menu_is_open());
        // Re-open; click the "取消" row (y+2) → Dismiss.
        s.open_menu(0, 0);
        assert_eq!(
            s.menu_click(MouseEventKind::Down(MouseButton::Left), 1, 2, 20, 20),
            Some(MenuClick::Dismiss)
        );
        // Click far outside the popup → None (falls through to selection).
        s.open_menu(0, 0);
        assert_eq!(
            s.menu_click(MouseEventKind::Down(MouseButton::Left), 15, 15, 20, 20),
            None
        );
        assert!(s.menu_is_open());
    }
}
