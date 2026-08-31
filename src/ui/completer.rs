//! 通用内联补全组件：统一 `@` mention 和 `/` slash picker 的逻辑。
//!
//! 通过 [`CompletableItem`] trait 抽象不同类型的补全项，消除 `app.rs` 中
//! 8 对重复的 mention/slash 方法。新增补全类型（如 `#` tag 补全）只需
//! 实现 trait 即可复用所有键盘交互逻辑。
//!
//! # 设计原则
//! - **高内聚**：所有补全逻辑集中在此模块
//! - **低耦合**：通过 trait 与具体类型解耦，不依赖 App
//! - **易扩展**：新增补全类型只需实现 CompletableItem

use std::path::PathBuf;
use crossterm::event::KeyCode;
use crate::ui::picker::{Picker, PickerKey, picker_key};
use crate::ui::mention::{self, Entry};

/// 可补全项 trait：定义补全项必须具备的能力
pub trait CompletableItem: Clone {
    /// 显示名称（用于列表展示）
    fn display_name(&self) -> &str;

    /// 是否匹配查询字符串（用于过滤）
    fn matches_query(&self, query: &str) -> bool;
}

/// 补全器动作：返回给调用方的指令
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleterAction<T> {
    /// 继续补全（无结果）
    Continue,
    /// 用户选中了一项
    Selected(T),
    /// 用户取消了补全
    Cancelled,
    /// 按键未处理（交给其他处理器）
    Unhandled(KeyCode),
}

/// 刷新回调函数类型：根据 prefix 返回过滤后的条目
///
/// # Arguments
/// * `prefix` - 当前输入的前缀
/// * `all_entries` - 所有可用条目
///
/// # Returns
/// 过滤后的条目列表
pub type RefreshFn<T> = Box<dyn Fn(&str, &[T]) -> Vec<T>>;

/// 通用内联补全组件
///
/// 封装了 picker 状态机和触发逻辑，提供统一的键盘处理接口。
/// 通过 `trigger` 字符（`@` 或 `/`）识别补全类型。
///
/// 支持自定义刷新逻辑，适用于需要特殊过滤规则的场景（如文件系统浏览）。
pub struct InlineCompleter<T: CompletableItem> {
    /// picker 状态（prefix + entries + selected）
    pub picker: Picker<T>,
    /// 触发字符（`@` 或 `/`）
    pub trigger: char,
    /// 所有可用条目（用于过滤）
    all_entries: Vec<T>,
    /// 自定义刷新逻辑（可选）
    refresh_fn: Option<RefreshFn<T>>,
}

impl<T: CompletableItem + std::fmt::Debug> std::fmt::Debug for InlineCompleter<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InlineCompleter")
            .field("picker", &self.picker)
            .field("trigger", &self.trigger)
            .field("all_entries", &self.all_entries)
            .field("refresh_fn", &self.refresh_fn.is_some())
            .finish()
    }
}

impl<T: CompletableItem> InlineCompleter<T> {
    /// 创建新的补全器（使用默认过滤逻辑）
    ///
    /// # Arguments
    /// * `trigger` - 触发字符（`@` 或 `/`）
    /// * `entries` - 所有可用条目
    pub fn new(trigger: char, entries: Vec<T>) -> Self {
        let mut picker = Picker::new();
        picker.entries = entries.clone();
        Self {
            picker,
            trigger,
            all_entries: entries,
            refresh_fn: None,
        }
    }

    /// 创建带自定义刷新逻辑的补全器
    ///
    /// # Arguments
    /// * `trigger` - 触发字符（`@` 或 `/`）
    /// * `entries` - 所有可用条目
    /// * `refresh_fn` - 自定义刷新逻辑
    pub fn with_refresh_fn(trigger: char, entries: Vec<T>, refresh_fn: RefreshFn<T>) -> Self {
        let mut picker = Picker::new();
        picker.entries = entries.clone();
        Self {
            picker,
            trigger,
            all_entries: entries,
            refresh_fn: Some(refresh_fn),
        }
    }

    /// 获取当前选中的条目
    pub fn selected_item(&self) -> Option<&T> {
        self.picker.entries.get(self.picker.selected)
    }

    /// 获取触发字符
    pub fn trigger(&self) -> char {
        self.trigger
    }

    /// 补全器是否为空（无输入）
    pub fn is_empty(&self) -> bool {
        self.picker.is_prefix_empty()
    }

    /// 处理键盘输入，返回动作
    ///
    /// 这是补全器的主要接口，统一处理所有按键逻辑。
    pub fn handle_key(&mut self, code: KeyCode) -> CompleterAction<T> {
        match picker_key(code) {
            Some(PickerKey::Cancel) => {
                CompleterAction::Cancelled
            }
            Some(PickerKey::Move(delta)) => {
                self.picker.move_selection(delta);
                CompleterAction::Continue
            }
            Some(PickerKey::Backspace) => {
                if self.picker.is_prefix_empty() {
                    // 前缀为空时，backspace 取消补全
                    CompleterAction::Cancelled
                } else {
                    self.picker.pop_char();
                    self.refresh_entries();
                    CompleterAction::Continue
                }
            }
            Some(PickerKey::Confirm) => {
                if let Some(item) = self.picker.entries.get(self.picker.selected) {
                    CompleterAction::Selected(item.clone())
                } else {
                    CompleterAction::Cancelled
                }
            }
            Some(PickerKey::Char(c)) => {
                self.picker.push_char(c);
                self.refresh_entries();
                CompleterAction::Continue
            }
            None => CompleterAction::Unhandled(code),
        }
    }

    /// 刷新过滤后的条目列表
    ///
    /// 根据当前 prefix 过滤 all_entries，更新 picker.entries。
    /// 如果设置了自定义刷新函数，优先使用它。
    fn refresh_entries(&mut self) {
        if let Some(ref refresh_fn) = self.refresh_fn {
            // 使用自定义刷新逻辑
            self.picker.entries = refresh_fn(&self.picker.prefix, &self.all_entries);
        } else {
            // 使用默认过滤逻辑
            if self.picker.prefix.is_empty() {
                self.picker.entries = self.all_entries.clone();
            } else {
                let query = &self.picker.prefix;
                self.picker.entries = self.all_entries
                    .iter()
                    .filter(|item| item.matches_query(query))
                    .cloned()
                    .collect();
            }
        }
        self.picker.clamp_selection();
    }

    /// 更新所有可用条目（用于动态加载）
    pub fn set_entries(&mut self, entries: Vec<T>) {
        self.all_entries = entries;
        self.refresh_entries();
    }

    /// 获取当前过滤后的条目数量
    pub fn entries_count(&self) -> usize {
        self.picker.entries.len()
    }

    /// 获取当前前缀
    pub fn prefix(&self) -> &str {
        &self.picker.prefix
    }

    /// 获取所有可用条目（用于外部更新）
    pub fn all_entries(&self) -> &[T] {
        &self.all_entries
    }
}

/// Mention 专用补全器：处理 `@` 文件路径补全的特殊逻辑
///
/// 主要特殊点：
/// 1. 总是添加一个合成的 "use what I typed" 条目（第一位）
/// 2. 需要 workspace_root 来解析相对路径
/// 3. 完成时需要生成正确的路径字符串
pub struct MentionCompleter {
    /// 内部使用 InlineCompleter
    inner: InlineCompleter<Entry>,
    /// 工作区根目录
    workspace_root: PathBuf,
    /// 当前解析到的目录
    current_dir: PathBuf,
}

impl std::fmt::Debug for MentionCompleter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MentionCompleter")
            .field("inner", &self.inner)
            .field("workspace_root", &self.workspace_root)
            .field("current_dir", &self.current_dir)
            .finish()
    }
}

impl MentionCompleter {
    /// 创建新的 MentionCompleter
    pub fn new(workspace_root: PathBuf) -> Self {
        // 创建一个占位的刷新函数（实际刷新逻辑在 refresh_entries 方法中）
        let refresh_fn = Box::new(|_prefix: &str, _entries: &[Entry]| -> Vec<Entry> {
            Vec::new()
        });

        let mut completer = Self {
            inner: InlineCompleter::with_refresh_fn('@', Vec::new(), refresh_fn),
            workspace_root: workspace_root.clone(),
            current_dir: workspace_root,
        };
        // 初始化时刷新条目
        completer.refresh_entries();
        completer
    }

    /// 获取当前选中的条目
    pub fn selected_item(&self) -> Option<&Entry> {
        self.inner.selected_item()
    }

    /// 获取触发字符
    pub fn trigger(&self) -> char {
        self.inner.trigger()
    }

    /// 补全器是否为空（无输入）
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 处理键盘输入，返回动作
    pub fn handle_key(&mut self, code: KeyCode) -> CompleterAction<Entry> {
        match self.inner.handle_key(code) {
            CompleterAction::Continue => {
                // 刷新条目（特殊逻辑）
                self.refresh_entries();
                CompleterAction::Continue
            }
            other => other,
        }
    }

    /// 刷新条目列表（包含合成条目）
    fn refresh_entries(&mut self) {
        let prefix = self.inner.picker.prefix.clone();
        let (dir, name) = mention::split_prefix(&self.workspace_root, &prefix);
        let mut entries = mention::list_entries(&dir, &name);

        // 添加合成的 "use what I typed" 条目（总是第一位）
        let full = if name.is_empty() { dir.clone() } else { dir.join(&name) };
        entries.insert(
            0,
            Entry {
                name: mention::rel_or_abs(&self.workspace_root, &full),
                path: full,
                is_dir: false,
                synthetic: true,
            },
        );

        self.current_dir = dir;
        self.inner.picker.entries = entries;
        self.inner.picker.clamp_selection();
    }

    /// 更新工作区根目录
    pub fn set_workspace_root(&mut self, root: PathBuf) {
        self.workspace_root = root;
    }

    /// 获取当前过滤后的条目数量
    pub fn entries_count(&self) -> usize {
        self.inner.entries_count()
    }

    /// 获取当前前缀
    pub fn prefix(&self) -> &str {
        self.inner.prefix()
    }

    /// 获取当前解析到的目录
    pub fn current_dir(&self) -> &PathBuf {
        &self.current_dir
    }

    /// 获取工作区根目录
    pub fn workspace_root(&self) -> &PathBuf {
        &self.workspace_root
    }

    /// 获取条目列表（用于渲染）
    pub fn entries(&self) -> &[Entry] {
        &self.inner.picker.entries
    }

    /// 获取当前选中索引（用于渲染）
    pub fn selected_index(&self) -> usize {
        self.inner.picker.selected
    }

    /// 生成完成时的文本（用于插入到 composer）
    pub fn finish_text(&self) -> String {
        self.inner.selected_item()
            .map(|e| mention::rel_or_abs(&self.workspace_root, &e.path))
            .unwrap_or_else(|| mention::rel_or_abs(&self.workspace_root, &self.current_dir))
    }
}

/// Slash 专用补全器：处理 `/` skill 补全的特殊逻辑
///
/// 主要特殊点：
/// 1. 来自 skill_summaries（动态加载）
/// 2. 过滤逻辑支持名称和描述
pub struct SlashCompleter {
    /// 内部使用 InlineCompleter
    inner: InlineCompleter<(String, String)>,
}

impl std::fmt::Debug for SlashCompleter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlashCompleter")
            .field("inner", &self.inner)
            .finish()
    }
}

impl SlashCompleter {
    /// 创建新的 SlashCompleter
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self {
            inner: InlineCompleter::new('/', entries),
        }
    }

    /// 获取当前选中的条目
    pub fn selected_item(&self) -> Option<&(String, String)> {
        self.inner.selected_item()
    }

    /// 获取触发字符
    pub fn trigger(&self) -> char {
        self.inner.trigger()
    }

    /// 补全器是否为空（无输入）
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 处理键盘输入，返回动作
    pub fn handle_key(&mut self, code: KeyCode) -> CompleterAction<(String, String)> {
        self.inner.handle_key(code)
    }

    /// 更新条目列表
    pub fn set_entries(&mut self, entries: Vec<(String, String)>) {
        self.inner.set_entries(entries);
    }

    /// 获取当前过滤后的条目数量
    pub fn entries_count(&self) -> usize {
        self.inner.entries_count()
    }

    /// 获取当前前缀
    pub fn prefix(&self) -> &str {
        self.inner.prefix()
    }

    /// 获取条目列表（用于渲染）
    pub fn entries(&self) -> &[(String, String)] {
        &self.inner.picker.entries
    }

    /// 获取当前选中索引（用于渲染）
    pub fn selected_index(&self) -> usize {
        self.inner.picker.selected
    }
}

/// 为 mention::Entry 实现 CompletableItem
impl CompletableItem for crate::ui::mention::Entry {
    fn display_name(&self) -> &str {
        &self.name
    }

    fn matches_query(&self, query: &str) -> bool {
        // 简单的前缀匹配（与原 mention 逻辑一致）
        self.name.to_lowercase().starts_with(&query.to_lowercase())
    }
}

/// 为 (String, String) 实现 CompletableItem（用于 slash picker）
///
/// 第一个元素是 name，第二个是 description
impl CompletableItem for (String, String) {
    fn display_name(&self) -> &str {
        &self.0
    }

    fn matches_query(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        self.0.to_lowercase().contains(&q) || self.1.to_lowercase().contains(&q)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用的补全项
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestItem {
        name: String,
        desc: String,
    }

    impl TestItem {
        fn new(name: &str, desc: &str) -> Self {
            Self {
                name: name.to_string(),
                desc: desc.to_string(),
            }
        }
    }

    impl CompletableItem for TestItem {
        fn display_name(&self) -> &str {
            &self.name
        }

        fn matches_query(&self, query: &str) -> bool {
            let q = query.to_lowercase();
            self.name.to_lowercase().contains(&q) || self.desc.to_lowercase().contains(&q)
        }
    }

    fn test_entries() -> Vec<TestItem> {
        vec![
            TestItem::new("file1.rs", "Rust source"),
            TestItem::new("file2.txt", "Text file"),
            TestItem::new("dir1", "Directory"),
            TestItem::new("main.rs", "Main entry"),
        ]
    }

    #[test]
    fn new_creates_empty_completer() {
        let completer = InlineCompleter::new('@', test_entries());
        assert_eq!(completer.trigger(), '@');
        assert!(completer.is_empty());
        assert_eq!(completer.entries_count(), 4);
    }

    #[test]
    fn push_char_filters_entries() {
        let mut completer = InlineCompleter::new('@', test_entries());

        // 输入 'f' 应该过滤到 file1.rs 和 file2.txt
        let action = completer.handle_key(KeyCode::Char('f'));
        assert_eq!(action, CompleterAction::Continue);
        assert_eq!(completer.entries_count(), 2);
        assert_eq!(completer.prefix(), "f");
    }

    #[test]
    fn backspace_removes_char() {
        let mut completer = InlineCompleter::new('@', test_entries());

        // 先输入 'f'
        completer.handle_key(KeyCode::Char('f'));
        assert_eq!(completer.entries_count(), 2);

        // backspace 应该恢复到全部条目
        let action = completer.handle_key(KeyCode::Backspace);
        assert_eq!(action, CompleterAction::Continue);
        assert_eq!(completer.entries_count(), 4);
        assert!(completer.is_empty());
    }

    #[test]
    fn backspace_on_empty_cancels() {
        let mut completer = InlineCompleter::new('@', test_entries());

        let action = completer.handle_key(KeyCode::Backspace);
        assert_eq!(action, CompleterAction::Cancelled);
    }

    #[test]
    fn esc_cancels() {
        let mut completer = InlineCompleter::new('@', test_entries());

        let action = completer.handle_key(KeyCode::Esc);
        assert_eq!(action, CompleterAction::Cancelled);
    }

    #[test]
    fn arrow_keys_move_selection() {
        let mut completer = InlineCompleter::new('@', test_entries());

        // Down 应该移动选中
        let action = completer.handle_key(KeyCode::Down);
        assert_eq!(action, CompleterAction::Continue);
        assert_eq!(completer.picker.selected, 1);

        // Up 应该移回来
        let action = completer.handle_key(KeyCode::Up);
        assert_eq!(action, CompleterAction::Continue);
        assert_eq!(completer.picker.selected, 0);
    }

    #[test]
    fn enter_selects_current_item() {
        let mut completer = InlineCompleter::new('@', test_entries());

        // 选中第一个
        let action = completer.handle_key(KeyCode::Enter);
        assert_eq!(action, CompleterAction::Selected(TestItem::new("file1.rs", "Rust source")));
    }

    #[test]
    fn enter_with_empty_entries_cancels() {
        let mut completer = InlineCompleter::<TestItem>::new('@', vec![]);

        let action = completer.handle_key(KeyCode::Enter);
        assert_eq!(action, CompleterAction::Cancelled);
    }

    #[test]
    fn set_entries_updates_list() {
        let mut completer = InlineCompleter::new('@', test_entries());
        assert_eq!(completer.entries_count(), 4);

        // 更新条目
        let new_entries = vec![TestItem::new("new.rs", "New file")];
        completer.set_entries(new_entries);
        assert_eq!(completer.entries_count(), 1);
    }

    #[test]
    fn unhandled_key_returns_unhandled() {
        let mut completer = InlineCompleter::new('@', test_entries());

        // Tab 键应该返回 Unhandled
        let action = completer.handle_key(KeyCode::Tab);
        assert_eq!(action, CompleterAction::Unhandled(KeyCode::Tab));
    }

    #[test]
    fn selected_item_returns_current() {
        let mut completer = InlineCompleter::new('@', test_entries());

        // 默认选中第一个
        assert_eq!(completer.selected_item(), Some(&TestItem::new("file1.rs", "Rust source")));

        // 移动后应该返回新的
        completer.handle_key(KeyCode::Down);
        assert_eq!(completer.selected_item(), Some(&TestItem::new("file2.txt", "Text file")));
    }

    #[test]
    fn selected_item_on_empty_returns_none() {
        let completer = InlineCompleter::<TestItem>::new('@', vec![]);
        assert_eq!(completer.selected_item(), None);
    }

    #[test]
    fn custom_refresh_fn_is_used() {
        // 自定义刷新逻辑：只返回以 "file" 开头的条目
        let refresh_fn = Box::new(|prefix: &str, entries: &[TestItem]| -> Vec<TestItem> {
            if prefix.is_empty() {
                entries.to_vec()
            } else {
                entries.iter()
                    .filter(|e| e.name.starts_with("file"))
                    .cloned()
                    .collect()
            }
        });

        let mut completer = InlineCompleter::with_refresh_fn('@', test_entries(), refresh_fn);

        // 输入 "x"，应该只返回 file1.rs 和 file2.txt（因为自定义逻辑）
        completer.handle_key(KeyCode::Char('x'));
        assert_eq!(completer.entries_count(), 2);
        assert_eq!(completer.prefix(), "x");
    }

    #[test]
    fn all_entries_accessor_works() {
        let completer = InlineCompleter::new('@', test_entries());
        assert_eq!(completer.all_entries().len(), 4);
    }

    #[test]
    fn mention_completer_creates_with_workspace_root() {
        let root = PathBuf::from("/tmp/test");
        let completer = MentionCompleter::new(root.clone());
        assert_eq!(completer.trigger(), '@');
        assert!(completer.is_empty());
        assert_eq!(completer.workspace_root(), &root);
    }

    #[test]
    fn slash_completer_creates_with_entries() {
        let entries = vec![
            ("skill1".to_string(), "Description 1".to_string()),
            ("skill2".to_string(), "Description 2".to_string()),
        ];
        let completer = SlashCompleter::new(entries);
        assert_eq!(completer.trigger(), '/');
        assert!(completer.is_empty());
        assert_eq!(completer.entries_count(), 2);
    }
}
