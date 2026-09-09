//! History tools for token-budget context management.
//!
//! Provides read-only access to previous context windows via 4 tools:
//! - `history.list_windows` — list all archived windows
//! - `history.list_items` — list messages in a window with filtering
//! - `history.read_item` — read a specific message's content
//! - `history.search_contents` — full-text search across windows
//!
//! These are "private" tools — the model uses them silently for context
//! continuity. The system prompt instructs the model never to disclose
//! them to the user.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use crate::history::HistoryStore;
use phi_agent::ContextWindowManager;

/// Token budget for tool output. Results exceeding this are truncated.
/// Must land safely below the framework's hard 16k-char tool-output cap
/// even for pure-ASCII content (4000 tokens ≈ 16k chars — session
/// 20260908_33f6a029 hit the cap exactly at that boundary).
const MAX_OUTPUT_TOKENS: usize = 3_500;

/// Truncate output text to fit within the token budget. Char-based (never
/// panics on multi-byte boundaries; CJK counts ~1.5 chars/token so CJK
/// output simply truncates less often).
fn truncate_to_tokens(text: &str, max_tokens: usize) -> String {
    let estimated = ContextWindowManager::estimate_tokens(text);
    if estimated <= max_tokens {
        return text.to_string();
    }
    let ratio = max_tokens as f64 / estimated as f64;
    let max_chars = ((text.chars().count() as f64) * ratio) as usize;
    let truncated: String = text.chars().take(max_chars).collect();
    format!(
        "{truncated}...\n[truncated - {estimated} tokens estimated, limit {max_tokens}; \
         use offset_chars/limit_chars to read the rest]"
    )
}

// ── history.list_windows ─────────────────────────────────────────────────────

/// List all archived context windows.
pub struct HistoryListWindowsTool {
    store: Arc<HistoryStore>,
}

impl HistoryListWindowsTool {
    pub fn new(store: Arc<HistoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for HistoryListWindowsTool {
    fn name(&self) -> &'static str {
        "history.list_windows"
    }

    fn description(&self) -> &'static str {
        "List archived context windows. Returns window IDs and message counts. Use to discover what history is available."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max windows to return. Default 20."
                },
                "recent_first": {
                    "type": "boolean",
                    "description": "Sort most recent first. Default false."
                }
            }
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "List archived context windows.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
        let recent_first = args
            .get("recent_first")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let meta = self.store.load_metadata();
        let mut windows = meta.windows.clone();

        if recent_first {
            windows.reverse();
        }
        windows.truncate(limit);

        if windows.is_empty() {
            return Ok(vec![Content::text("No archived windows found.".to_string())]);
        }

        let mut output = String::from("Archived context windows:\n");
        for w in &windows {
            output.push_str(&format!(
                "  Window {}: {} messages, archived at {}\n",
                w.id, w.message_count, w.started_at
            ));
        }

        Ok(vec![Content::text(truncate_to_tokens(&output, MAX_OUTPUT_TOKENS))])
    }
}

// ── history.list_items ───────────────────────────────────────────────────────

/// List messages in a window with optional filtering.
pub struct HistoryListItemsTool {
    store: Arc<HistoryStore>,
}

impl HistoryListItemsTool {
    pub fn new(store: Arc<HistoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for HistoryListItemsTool {
    fn name(&self) -> &'static str {
        "history.list_items"
    }

    fn description(&self) -> &'static str {
        "List messages in archived context windows. Filter by role (user/assistant/tool/system) or tool name. Returns message summaries with role and content preview."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "window_id": {
                    "type": "integer",
                    "description": "Window ID to list from. Omit to list from all windows."
                },
                "role": {
                    "type": "string",
                    "description": "Filter by message role: user, assistant, tool, system."
                },
                "tool_name": {
                    "type": "string",
                    "description": "Filter by tool name (only for role=tool messages)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max items to return. Default 50."
                },
                "recent_first": {
                    "type": "boolean",
                    "description": "Sort most recent first. Default false."
                }
            }
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "List messages in archived context windows.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let window_id = args.get("window_id").and_then(Value::as_u64).map(|v| v as usize);
        let role = args.get("role").and_then(Value::as_str);
        let tool_name = args.get("tool_name").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
        let recent_first = args
            .get("recent_first")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let items = self.store.list_items(window_id, role, tool_name, limit, recent_first);

        if items.is_empty() {
            return Ok(vec![Content::text("No matching messages found.".to_string())]);
        }

        let mut output = String::new();
        for item in &items {
            let name_suffix = item
                .name
                .as_ref()
                .map(|n| format!(" ({})", n))
                .unwrap_or_default();
            output.push_str(&format!(
                "[W{:03}#{}] {}{}: {}\n",
                item.window, item.index, item.role, name_suffix, item.content_preview
            ));
        }

        Ok(vec![Content::text(truncate_to_tokens(&output, MAX_OUTPUT_TOKENS))])
    }
}

// ── history.read_item ────────────────────────────────────────────────────────

/// Read a specific message's content by window ID and item index.
pub struct HistoryReadItemTool {
    store: Arc<HistoryStore>,
}

impl HistoryReadItemTool {
    pub fn new(store: Arc<HistoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for HistoryReadItemTool {
    fn name(&self) -> &'static str {
        "history.read_item"
    }

    fn description(&self) -> &'static str {
        "Read a specific message from history by window ID and item index. Supports reading a character range for large messages."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "window_id": {
                    "type": "integer",
                    "description": "Window ID (as shown by list_windows)."
                },
                "item_id": {
                    "type": "integer",
                    "description": "Item index within the window (as shown by list_items)."
                },
                "offset_chars": {
                    "type": "integer",
                    "description": "Character offset to start reading from. Default 0."
                },
                "limit_chars": {
                    "type": "integer",
                    "description": "Max characters to read. Default: full message."
                }
            },
            "required": ["window_id", "item_id"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Read a specific message from history.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let window_id = match args.get("window_id").and_then(Value::as_u64) {
            Some(v) => v as usize,
            None => return Ok(vec![Content::text("[Error]: window_id is required".to_string())]),
        };
        let item_id = match args.get("item_id").and_then(Value::as_u64) {
            Some(v) => v as usize,
            None => return Ok(vec![Content::text("[Error]: item_id is required".to_string())]),
        };
        let offset_chars = args
            .get("offset_chars")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let limit_chars = args
            .get("limit_chars")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        match self.store.read_item(window_id, item_id, offset_chars, limit_chars) {
            Some(content) => Ok(vec![Content::text(truncate_to_tokens(&content, MAX_OUTPUT_TOKENS))]),
            None => Ok(vec![Content::text(format!(
                "Item not found: window {}, item {}",
                window_id, item_id
            ))]),
        }
    }
}

// ── history.search_contents ──────────────────────────────────────────────────

/// Search content across archived windows.
pub struct HistorySearchContentsTool {
    store: Arc<HistoryStore>,
}

impl HistorySearchContentsTool {
    pub fn new(store: Arc<HistoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for HistorySearchContentsTool {
    fn name(&self) -> &'static str {
        "history.search_contents"
    }

    fn description(&self) -> &'static str {
        "Search message content across archived windows. Case-insensitive substring match. Returns matching snippets with window/item references."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search string (case-insensitive substring match)."
                },
                "window_id": {
                    "type": "integer",
                    "description": "Restrict search to this window. Omit to search all."
                },
                "role": {
                    "type": "string",
                    "description": "Filter by message role."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results. Default 10."
                }
            },
            "required": ["query"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Search content across archived windows.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) => q.trim(),
            None => return Ok(vec![Content::text("[Error]: query is required".to_string())]),
        };
        if query.is_empty() {
            return Ok(vec![Content::text("[Error]: query cannot be empty".to_string())]);
        }

        let window_id = args.get("window_id").and_then(Value::as_u64).map(|v| v as usize);
        let role = args.get("role").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;

        let results = self.store.search_contents(query, window_id, role, limit);

        if results.is_empty() {
            return Ok(vec![Content::text(format!(
                "No matches found for '{}'",
                query
            ))]);
        }

        let mut output = format!("Search results for '{}':\n", query);
        for r in &results {
            let name_suffix = r
                .name
                .as_ref()
                .map(|n| format!(" ({})", n))
                .unwrap_or_default();
            output.push_str(&format!(
                "\n[W{:03}#{}] {}{}:\n{}\n",
                r.window, r.index, r.role, name_suffix, r.match_preview
            ));
        }

        Ok(vec![Content::text(truncate_to_tokens(&output, MAX_OUTPUT_TOKENS))])
    }
}

// ── Registration helper ─────────────────────────────────────────────────────

/// Create all 4 history tools from a shared store.
pub fn create_history_tools(
    store: Arc<HistoryStore>,
) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(HistoryListWindowsTool::new(Arc::clone(&store))),
        Box::new(HistoryListItemsTool::new(Arc::clone(&store))),
        Box::new(HistoryReadItemTool::new(Arc::clone(&store))),
        Box::new(HistorySearchContentsTool::new(store)),
    ]
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::ChatMessage;
    use tempfile::TempDir;

    fn ctx() -> ToolContext {
        ToolContext::for_test()
    }

    /// The truncated output must stay safely below the framework's hard
    /// 16k-char tool-output cap, for ASCII and CJK alike (and never panic
    /// slicing through a multi-byte char).
    #[test]
    fn truncate_stays_below_framework_cap_and_never_panics() {
        // ASCII: 40k chars ≈ 10k estimated tokens → truncates.
        let ascii = "x".repeat(40_000);
        let out = truncate_to_tokens(&ascii, MAX_OUTPUT_TOKENS);
        assert!(out.chars().count() < 16_000, "ascii len {}", out.len());

        // CJK: 30k chars ≈ 20k estimated tokens → truncates mid-char safely.
        let cjk = "中".repeat(30_000);
        let out = truncate_to_tokens(&cjk, MAX_OUTPUT_TOKENS);
        assert!(out.chars().count() < 16_000, "cjk len {}", out.len());
        assert!(out.contains("[truncated"));
    }

    fn text(out: &[Content]) -> String {
        out.iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn setup_store(tmp: &TempDir) -> Arc<HistoryStore> {
        let store = Arc::new(HistoryStore::new(tmp.path(), "test"));
        store
            .archive_window(
                1,
                &[
                    ChatMessage::user("fix the login bug"),
                    ChatMessage::assistant("I'll investigate"),
                    ChatMessage::tool_with_name("tc1", "shell", "error: connection refused"),
                ],
            )
            .unwrap();
        store
    }

    #[tokio::test]
    async fn list_windows_returns_archived() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListWindowsTool::new(store);

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        assert!(text(&out).contains("Window 001"));
        assert!(text(&out).contains("3 messages"));
    }

    #[tokio::test]
    async fn list_windows_empty() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(HistoryStore::new(tmp.path(), "test"));
        let tool = HistoryListWindowsTool::new(store);

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        assert!(text(&out).contains("No archived windows"));
    }

    #[tokio::test]
    async fn list_items_returns_all() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListItemsTool::new(store);

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        let t = text(&out);
        assert!(t.contains("user"));
        assert!(t.contains("assistant"));
        assert!(t.contains("tool"));
    }

    #[tokio::test]
    async fn list_items_filters_by_role() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListItemsTool::new(store);

        let out = tool
            .call(&json!({"role": "user"}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("user"));
        assert!(!t.contains("assistant"));
    }

    #[tokio::test]
    async fn read_item_returns_content() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryReadItemTool::new(store);

        let out = tool
            .call(&json!({"window_id": 1, "item_id": 0}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("fix the login bug"));
    }

    #[tokio::test]
    async fn read_item_with_offset() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryReadItemTool::new(store);

        let out = tool
            .call(&json!({"window_id": 1, "item_id": 0, "offset_chars": 8}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("login bug"));
    }

    #[tokio::test]
    async fn read_item_not_found() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryReadItemTool::new(store);

        let out = tool
            .call(&json!({"window_id": 99, "item_id": 0}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("not found"));
    }

    #[tokio::test]
    async fn search_finds_match() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistorySearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "login"}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("login"));
        assert!(t.contains("W001#0"));
    }

    #[tokio::test]
    async fn search_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistorySearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "LOGIN"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("login"));
    }

    #[tokio::test]
    async fn search_empty_query_returns_error() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistorySearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "  "}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("[Error]"));
    }

    #[tokio::test]
    async fn search_no_match() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistorySearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "nonexistent"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("No matches"));
    }

    #[test]
    fn create_history_tools_returns_four() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(HistoryStore::new(tmp.path(), "test"));
        let tools = create_history_tools(store);
        assert_eq!(tools.len(), 4);
    }

    // ── list_items: filtering ───────────────────────────────────────────

    #[tokio::test]
    async fn list_items_filters_by_window_id() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        // Archive a second window
        store
            .archive_window(2, &[ChatMessage::user("second window msg")])
            .unwrap();
        let tool = HistoryListItemsTool::new(store);

        let out = tool
            .call(&json!({"window_id": 1}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("fix the login bug"));
        assert!(!t.contains("second window msg"));
    }

    #[tokio::test]
    async fn list_items_filters_by_tool_name() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListItemsTool::new(store);

        let out = tool
            .call(&json!({"tool_name": "shell"}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("tool"));
        assert!(t.contains("shell"));
        assert!(!t.contains("user"));
    }

    #[tokio::test]
    async fn list_items_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListItemsTool::new(store);

        let out = tool
            .call(&json!({"limit": 1}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        // Should have exactly 1 item line (plus no truncation marker)
        let item_lines: Vec<&str> = t.lines().filter(|l| l.starts_with("[W")).collect();
        assert_eq!(item_lines.len(), 1);
    }

    #[tokio::test]
    async fn list_items_recent_first() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListItemsTool::new(store);

        let out = tool
            .call(&json!({"recent_first": true}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        // Last item (tool) should appear first
        let first_line = t.lines().find(|l| l.starts_with("[W")).unwrap();
        assert!(first_line.contains("tool"), "first line: {}", first_line);
    }

    #[tokio::test]
    async fn list_items_empty_for_no_match() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryListItemsTool::new(store);

        let out = tool
            .call(&json!({"role": "nonexistent"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("No matching"));
    }

    #[tokio::test]
    async fn list_items_across_multiple_windows() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        store
            .archive_window(2, &[ChatMessage::user("window2 msg")])
            .unwrap();
        let tool = HistoryListItemsTool::new(store);

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        let t = text(&out);
        assert!(t.contains("fix the login bug"));
        assert!(t.contains("window2 msg"));
    }

    // ── read_item: limit_chars ──────────────────────────────────────────

    #[tokio::test]
    async fn read_item_with_limit_chars() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistoryReadItemTool::new(store);

        let out = tool
            .call(&json!({"window_id": 1, "item_id": 0, "limit_chars": 5}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("fix t"), "got: {}", t);
    }

    // ── search: path filter ─────────────────────────────────────────────

    #[tokio::test]
    async fn search_finds_by_content() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = HistorySearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "connection"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("connection"));
    }

    // ── Concurrency: parallel tool execution ────────────────────────────
    // This is the critical test — the hang bug occurs when history tools
    // run in parallel via join_all (phase2 of tool_engine).

    #[tokio::test]
    async fn concurrent_list_items_does_not_hang() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        // Add more windows to stress the concurrent reads
        for i in 2..=5 {
            store
                .archive_window(
                    i,
                    &[ChatMessage::user(&format!("window{i} msg"))],
                )
                .unwrap();
        }

        let tool = Arc::new(HistoryListItemsTool::new(store));

        // Spawn 4 concurrent calls — mimics tool_engine phase2
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let tool = Arc::clone(&tool);
                tokio::spawn(async move {
                    tool.call(&json!({}), &ctx()).await
                })
            })
            .collect();

        // If any call hangs, the timeout fires
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures_util::future::join_all(handles),
        )
        .await;

        assert!(results.is_ok(), "concurrent list_items timed out (hang!)");
        for r in results.unwrap() {
            let out = r.unwrap().unwrap();
            assert!(!text(&out).is_empty());
        }
    }

    #[tokio::test]
    async fn concurrent_mixed_history_tools_do_not_hang() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        for i in 2..=3 {
            store
                .archive_window(i, &[ChatMessage::user(&format!("w{i}"))])
                .unwrap();
        }

        let list_windows = Arc::new(HistoryListWindowsTool::new(Arc::clone(&store)));
        let list_items = Arc::new(HistoryListItemsTool::new(Arc::clone(&store)));
        let read_item = Arc::new(HistoryReadItemTool::new(Arc::clone(&store)));
        let search = Arc::new(HistorySearchContentsTool::new(store));

        // Run all 4 history tools in parallel
        let futs = vec![
            tokio::spawn({
                let t = Arc::clone(&list_windows);
                async move { t.call(&json!({}), &ctx()).await }
            }),
            tokio::spawn({
                let t = Arc::clone(&list_items);
                async move { t.call(&json!({}), &ctx()).await }
            }),
            tokio::spawn({
                let t = Arc::clone(&read_item);
                async move { t.call(&json!({"window_id": 1, "item_id": 0}), &ctx()).await }
            }),
            tokio::spawn({
                let t = Arc::clone(&search);
                async move { t.call(&json!({"query": "login"}), &ctx()).await }
            }),
        ];

        let results = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            futures_util::future::join_all(futs),
        )
        .await;

        assert!(results.is_ok(), "concurrent mixed tools timed out (hang!)");
        for r in results.unwrap() {
            let out = r.unwrap().unwrap();
            assert!(!text(&out).is_empty());
        }
    }

}
