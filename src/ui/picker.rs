//! 通用 picker 状态机：`@` mention 和 `/` skill 两个 picker 共享的机械部分。
//!
//! 每个 picker 都是「typed prefix + 过滤后的 entries + 高亮 selected」三元组，
//! 键盘交互完全一致（Esc 取消 / Up·Down 移动 / Backspace 删前缀 / Enter 确认 /
//! 普通字符追加）。这里抽的是纯状态操作；App 层负责两个差异点：如何从 prefix
//! 生成 entries（文件列表 vs skill 过滤）、确认时往 composer 插入什么。

use crossterm::event::KeyCode;

/// 一次 picker 键盘输入对应的机械动作，由 App 层解释执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    /// 普通字符：追加到 prefix。
    Char(char),
    /// 退格：删除最后一个前缀字符（空前缀时由 App 决定是否关闭 picker）。
    Backspace,
    /// 移动高亮（±1）。
    Move(i32),
    /// 确认当前选中。
    Confirm,
    /// 取消整个 picker。
    Cancel,
}

/// 把一次按键翻译成 [`PickerKey`]；无关按键返回 `None`（调用方吞掉）。
pub fn picker_key(code: KeyCode) -> Option<PickerKey> {
    use KeyCode::*;
    match code {
        Esc => Some(PickerKey::Cancel),
        Up => Some(PickerKey::Move(-1)),
        Down => Some(PickerKey::Move(1)),
        Backspace => Some(PickerKey::Backspace),
        Enter => Some(PickerKey::Confirm),
        Char(c) => Some(PickerKey::Char(c)),
        _ => None,
    }
}

/// 通用 picker 状态：typed prefix、过滤后的 entries、高亮索引。
#[derive(Debug, Clone, Default)]
pub struct Picker<T> {
    pub prefix: String,
    pub entries: Vec<T>,
    pub selected: usize,
}

impl<T> Picker<T> {
    pub fn new() -> Self {
        Self {
            prefix: String::new(),
            entries: Vec::new(),
            selected: 0,
        }
    }

    /// True when no prefix has been typed. A backspace on an empty prefix
    /// should close the picker (removing the `@`/`/` trigger) rather than
    /// delete a prefix char.
    pub fn is_prefix_empty(&self) -> bool {
        self.prefix.is_empty()
    }

    /// Append a char to the prefix and reset the highlight to the top.
    pub fn push_char(&mut self, c: char) {
        self.prefix.push(c);
        self.selected = 0;
    }

    /// Remove the last prefix char and reset the highlight to the top.
    pub fn pop_char(&mut self) {
        self.prefix.pop();
        self.selected = 0;
    }

    /// Move the highlight by `delta` (±1), clamped into the entry list.
    /// No-op when there are no entries.
    pub fn move_selection(&mut self, delta: i32) {
        let n = self.entries.len() as i32;
        if n == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).clamp(0, n - 1) as usize;
    }

    /// Clamp `selected` into the current entry range (call after refreshing
    /// `entries`, since a filter can shrink the list).
    pub fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker_with(items: &[&'static str]) -> Picker<&'static str> {
        Picker {
            prefix: String::new(),
            entries: items.to_vec(),
            selected: 0,
        }
    }

    #[test]
    fn push_and_pop_edit_prefix_and_reset_selection() {
        let mut p = picker_with(&["a", "b", "c"]);
        p.selected = 2;
        p.push_char('x');
        assert_eq!(p.prefix, "x");
        assert_eq!(p.selected, 0);
        p.pop_char();
        assert!(p.is_prefix_empty());
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn move_selection_clamps_to_entry_range() {
        let mut p = picker_with(&["a", "b", "c"]);
        p.move_selection(-5);
        assert_eq!(p.selected, 0);
        p.move_selection(10);
        assert_eq!(p.selected, 2);
        // Empty list: no-op.
        let mut empty = Picker::<&str>::new();
        empty.move_selection(1);
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn clamp_selection_after_shrink() {
        let mut p = picker_with(&["a", "b", "c"]);
        p.selected = 2;
        p.entries = vec!["a"];
        p.clamp_selection();
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn picker_key_maps_bindings_and_ignores_others() {
        use KeyCode::*;
        assert_eq!(picker_key(Esc), Some(PickerKey::Cancel));
        assert_eq!(picker_key(Up), Some(PickerKey::Move(-1)));
        assert_eq!(picker_key(Down), Some(PickerKey::Move(1)));
        assert_eq!(picker_key(Backspace), Some(PickerKey::Backspace));
        assert_eq!(picker_key(Enter), Some(PickerKey::Confirm));
        assert_eq!(picker_key(Char('q')), Some(PickerKey::Char('q')));
        assert_eq!(picker_key(Left), None);
        assert_eq!(picker_key(Home), None);
    }
}
