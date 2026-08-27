//! 滚动视图状态：`scroll_offset` / `follow_bottom` 和可见窗口计算。
//!
//! 从 `App` god-object 抽出（此前是 App 上的两个字段 + `scroll_up` /
//! `scroll_down` / `window_range`）。App 持有一个实例：渲染层每帧用它算可见
//! 窗口，鼠标滚轮通过 `scroll_up`/`scroll_down` 移动视口。

use std::ops::Range;

/// The scrollable output viewport: how far the transcript is scrolled up from
/// the bottom, whether it is pinned to the newest lines, and the last known
/// rendered size (used to clamp the offset to the useful range).
#[derive(Debug)]
pub struct Viewport {
    /// Lines scrolled up from the bottom (`0` while following the bottom).
    pub scroll_offset: usize,
    /// Whether the output view is pinned to the newest lines.
    pub follow_bottom: bool,
    /// Last known rendered line count (updated by `render_output` each frame).
    pub rendered_total: usize,
    /// Last known output-pane height in rows (updated by `render_output`).
    /// `0` before the first render — scroll_up then cannot clamp yet.
    pub viewport_height: usize,
}

impl Viewport {
    pub fn new() -> Self {
        Self {
            scroll_offset: 0,
            follow_bottom: true,
            rendered_total: 0,
            viewport_height: 0,
        }
    }

    /// Record the last rendered total + pane height (from `render_output`).
    pub fn set_visible(&mut self, total: usize, height: usize) {
        self.rendered_total = total;
        self.viewport_height = height;
    }

    /// Scroll up by `step` lines. Returns `true` if the viewport moved.
    /// Once the viewport knows its rendered size, the offset is clamped to the
    /// maximum useful offset (past the top of the content).
    pub fn scroll_up(&mut self, step: usize) -> bool {
        let old = self.scroll_offset;
        self.follow_bottom = false;
        self.scroll_offset += step;
        // Clamp to the maximum useful offset using the last known rendered
        // line count and pane height. This prevents scroll_offset from
        // growing past the top of content.
        if self.viewport_height > 0 {
            let max_offset = self.rendered_total.saturating_sub(self.viewport_height);
            if self.scroll_offset > max_offset {
                self.scroll_offset = max_offset;
            }
        }
        tracing::debug!(
            old_offset = old,
            new_offset = self.scroll_offset,
            rendered_total = self.rendered_total,
            viewport_height = self.viewport_height,
            "scroll_up"
        );
        self.scroll_offset != old
    }

    /// Scroll down by `step` lines. Returns `true` if the viewport moved;
    /// re-enters follow-bottom when the scroll reaches the bottom.
    pub fn scroll_down(&mut self, step: usize) -> bool {
        if self.follow_bottom {
            tracing::debug!("scroll_down: already at bottom");
            return false;
        }
        let old = self.scroll_offset;
        if self.scroll_offset <= step {
            self.scroll_offset = 0;
            self.follow_bottom = true;
        } else {
            self.scroll_offset -= step;
        }
        tracing::debug!(
            old_offset = old,
            new_offset = self.scroll_offset,
            follow_bottom = self.follow_bottom,
            "scroll_down"
        );
        true
    }

    /// The `[start, end)` range of `total` lines to show in a window of
    /// `height` rows, honoring `follow_bottom` and `scroll_offset`.
    pub fn window_range(&self, total: usize, height: usize) -> Range<usize> {
        if total <= height {
            tracing::debug!(total, height, "window_range: content fits, 0..total");
            return 0..total;
        }
        if self.follow_bottom {
            let start = total - height;
            tracing::debug!(total, height, start, "window_range: follow_bottom");
            return start..total;
        }
        let max_offset = total - height;
        let offset = self.scroll_offset.min(max_offset);
        let start = max_offset - offset;
        tracing::debug!(
            total,
            height,
            raw_scroll_offset = self.scroll_offset,
            max_offset,
            clamped_offset = offset,
            start,
            end = (start + height).min(total),
            "window_range"
        );
        start..(start + height).min(total)
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_range_follows_bottom_when_pinned() {
        let v = Viewport::new();
        assert_eq!(v.window_range(100, 30), 70..100);
        // Content fits: 0..total regardless of scroll.
        assert_eq!(v.window_range(10, 30), 0..10);
    }

    #[test]
    fn window_range_honors_scroll_offset() {
        let mut v = Viewport::new();
        v.follow_bottom = false;
        v.scroll_offset = 10;
        assert_eq!(v.window_range(100, 30), 60..90);
        // Past the top clamps to the first `height` lines.
        v.scroll_offset = 1000;
        assert_eq!(v.window_range(100, 30), 0..30);
    }

    #[test]
    fn scroll_up_leaves_follow_bottom() {
        let mut v = Viewport::new();
        assert!(v.follow_bottom);
        assert!(v.scroll_up(1));
        assert!(!v.follow_bottom);
        assert_eq!(v.scroll_offset, 1);
    }

    #[test]
    fn scroll_up_clamps_to_rendered_top() {
        let mut v = Viewport::new();
        v.set_visible(100, 30);
        v.follow_bottom = false;
        v.scroll_offset = 60;
        // 60 + 1 would exceed max_offset (100-30=70)? No: 61 <= 70, stays.
        assert!(v.scroll_up(1));
        assert_eq!(v.scroll_offset, 61);
        // Past the top clamps to the first pane.
        v.scroll_offset = 69;
        assert!(v.scroll_up(5));
        assert_eq!(v.scroll_offset, 70);
        assert!(!v.scroll_up(1), "already at the top: no movement");
    }

    #[test]
    fn scroll_down_reenters_follow_bottom() {
        let mut v = Viewport::new();
        v.scroll_up(2);
        assert_eq!(v.scroll_offset, 2);
        // Down one step: still scrolled up.
        assert!(v.scroll_down(1));
        assert_eq!(v.scroll_offset, 1);
        assert!(!v.follow_bottom);
        // Down the remaining step: back at bottom.
        assert!(v.scroll_down(1));
        assert!(v.follow_bottom);
        assert_eq!(v.scroll_offset, 0);
        // Already at bottom: no-op.
        assert!(!v.scroll_down(1));
    }
}
