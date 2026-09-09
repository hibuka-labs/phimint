//! Notes tools for token-budget context management.
//!
//! Provides read-write access to a persistent scratchpad via 5 tools:
//! - `notes.list_files` — list note files
//! - `notes.read_file` — read a note file
//! - `notes.search_contents` — search across notes
//! - `notes.append_to_file` — append to a note
//! - `notes.write_file` — create/overwrite a note
//!
//! These are "private" tools — the model uses them silently for state
//! persistence across context windows.

use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use crate::notes::NotesStore;

// ── notes.list_files ────────────────────────────────────────────────────────

/// List note files under an optional prefix.
pub struct NotesListFilesTool {
    store: Arc<NotesStore>,
}

impl NotesListFilesTool {
    pub fn new(store: Arc<NotesStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for NotesListFilesTool {
    fn name(&self) -> &'static str {
        "notes.list_files"
    }

    fn description(&self) -> &'static str {
        "List note files. This is your private scratchpad — never show to the user. Use prefix to narrow results."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prefix": {
                    "type": "string",
                    "description": "Optional path prefix to filter by, e.g. 'progress' or 'sub/dir'."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max files to return. Default 50."
                }
            }
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "List note files.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let prefix = args.get("prefix").and_then(Value::as_str);
        let max = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(50) as usize;

        let files = self.store.list_files(prefix, max);

        if files.is_empty() {
            return Ok(vec![Content::text("No note files found.".to_string())]);
        }

        let mut output = format!("Note files ({}):\n", files.len());
        for f in &files {
            output.push_str(&format!("  {}\n", f));
        }
        Ok(vec![Content::text(output)])
    }
}

// ── notes.read_file ─────────────────────────────────────────────────────────

/// Read a note file with optional line range.
pub struct NotesReadFileTool {
    store: Arc<NotesStore>,
}

impl NotesReadFileTool {
    pub fn new(store: Arc<NotesStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for NotesReadFileTool {
    fn name(&self) -> &'static str {
        "notes.read_file"
    }

    fn description(&self) -> &'static str {
        "Read a note file. This is your private scratchpad — never show to the user. Supports line range for large files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to notes root, e.g. 'progress.md'."
                },
                "start_line": {
                    "type": "integer",
                    "description": "1-indexed start line. Default 1."
                },
                "stop_line": {
                    "type": "integer",
                    "description": "1-indexed stop line (inclusive). Default: end of file."
                }
            },
            "required": ["path"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Read a note file.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return Ok(vec![Content::text("[Error]: path is required".to_string())]),
        };
        let start_line = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);
        let stop_line = args
            .get("stop_line")
            .and_then(Value::as_u64)
            .map(|v| v as usize);

        match self.store.read_file(path, start_line, stop_line) {
            Ok(content) => Ok(vec![Content::text(content)]),
            Err(e) => Ok(vec![Content::text(format!("[Error]: {}", e))]),
        }
    }
}

// ── notes.search_contents ───────────────────────────────────────────────────

/// Search across note files.
pub struct NotesSearchContentsTool {
    store: Arc<NotesStore>,
}

impl NotesSearchContentsTool {
    pub fn new(store: Arc<NotesStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for NotesSearchContentsTool {
    fn name(&self) -> &'static str {
        "notes.search_contents"
    }

    fn description(&self) -> &'static str {
        "Search note file contents. Case-insensitive substring match. Returns matching lines with file paths."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search string (case-insensitive)."
                },
                "prefix": {
                    "type": "string",
                    "description": "Restrict to files under this prefix."
                },
                "max_files": {
                    "type": "integer",
                    "description": "Max files to search. Default 20."
                },
                "max_matches": {
                    "type": "integer",
                    "description": "Max matching lines per file. Default 5."
                }
            },
            "required": ["query"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Search note file contents.".to_string(),
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

        let prefix = args.get("prefix").and_then(Value::as_str);
        let max_files = args
            .get("max_files")
            .and_then(Value::as_u64)
            .unwrap_or(20) as usize;
        let max_matches = args
            .get("max_matches")
            .and_then(Value::as_u64)
            .unwrap_or(5) as usize;

        let results = self
            .store
            .search_contents(query, prefix, max_files, max_matches);

        if results.is_empty() {
            return Ok(vec![Content::text(format!(
                "No matches found for '{}'",
                query
            ))]);
        }

        let mut output = format!("Search results for '{}':\n", query);
        for r in &results {
            output.push_str(&format!("\n{}:\n", r.path));
            for m in &r.matches {
                output.push_str(&format!("  L{}: {}\n", m.line_number, m.line_content));
            }
        }

        Ok(vec![Content::text(output)])
    }
}

// ── notes.append_to_file ────────────────────────────────────────────────────

/// Append content to a note file.
pub struct NotesAppendToFileTool {
    store: Arc<NotesStore>,
}

impl NotesAppendToFileTool {
    pub fn new(store: Arc<NotesStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for NotesAppendToFileTool {
    fn name(&self) -> &'static str {
        "notes.append_to_file"
    }

    fn description(&self) -> &'static str {
        "Append content to a note file (creates if missing). This is your private scratchpad — never show to the user. Max 1MB per file."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to notes root."
                },
                "text": {
                    "type": "string",
                    "description": "Content to append."
                }
            },
            "required": ["path", "text"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Append to a note file.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return Ok(vec![Content::text("[Error]: path is required".to_string())]),
        };
        let text = match args.get("text").and_then(Value::as_str) {
            Some(t) => t,
            None => return Ok(vec![Content::text("[Error]: text is required".to_string())]),
        };

        match self.store.append_to_file(path, text) {
            Ok(()) => Ok(vec![Content::text(format!(
                "Appended to {}",
                path
            ))]),
            Err(e) => Ok(vec![Content::text(format!("[Error]: {}", e))]),
        }
    }
}

// ── notes.write_file ────────────────────────────────────────────────────────

/// Create or overwrite a note file.
pub struct NotesWriteFileTool {
    store: Arc<NotesStore>,
}

impl NotesWriteFileTool {
    pub fn new(store: Arc<NotesStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for NotesWriteFileTool {
    fn name(&self) -> &'static str {
        "notes.write_file"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a note file. This is your private scratchpad — never show to the user. Max 1MB per file. Use append_to_file to add to existing files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to notes root."
                },
                "text": {
                    "type": "string",
                    "description": "Content to write (replaces existing)."
                }
            },
            "required": ["path", "text"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Create or overwrite a note file.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return Ok(vec![Content::text("[Error]: path is required".to_string())]),
        };
        let text = match args.get("text").and_then(Value::as_str) {
            Some(t) => t,
            None => return Ok(vec![Content::text("[Error]: text is required".to_string())]),
        };

        match self.store.write_file(path, text) {
            Ok(()) => Ok(vec![Content::text(format!("Wrote {}", path))]),
            Err(e) => Ok(vec![Content::text(format!("[Error]: {}", e))]),
        }
    }
}

// ── Registration helper ─────────────────────────────────────────────────────

/// Create all 5 notes tools from a shared store.
pub fn create_notes_tools(store: Arc<NotesStore>) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(NotesListFilesTool::new(Arc::clone(&store))),
        Box::new(NotesReadFileTool::new(Arc::clone(&store))),
        Box::new(NotesSearchContentsTool::new(Arc::clone(&store))),
        Box::new(NotesAppendToFileTool::new(Arc::clone(&store))),
        Box::new(NotesWriteFileTool::new(store)),
    ]
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ctx() -> ToolContext {
        ToolContext::for_test()
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

    fn setup_store(tmp: &TempDir) -> Arc<NotesStore> {
        let store = Arc::new(NotesStore::new(tmp.path(), "test", "main"));
        store.write_file("progress.md", "task 1 done\ntask 2 pending").unwrap();
        store.write_file("findings.md", "found a bug in login").unwrap();
        store
    }

    #[tokio::test]
    async fn list_files_returns_all() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesListFilesTool::new(store);

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        let t = text(&out);
        assert!(t.contains("progress.md"));
        assert!(t.contains("findings.md"));
    }

    #[tokio::test]
    async fn list_files_with_prefix() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(NotesStore::new(tmp.path(), "test", "main"));
        store.write_file("dir/a.md", "a").unwrap();
        store.write_file("dir/b.md", "b").unwrap();
        store.write_file("other/c.md", "c").unwrap();

        let tool = NotesListFilesTool::new(store);
        let out = tool.call(&json!({"prefix": "dir"}), &ctx()).await.unwrap();
        let t = text(&out);
        assert!(t.contains("dir/a.md"));
        assert!(!t.contains("other/c.md"));
    }

    #[tokio::test]
    async fn read_file_returns_content() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesReadFileTool::new(store);

        let out = tool
            .call(&json!({"path": "progress.md"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("task 1 done"));
    }

    #[tokio::test]
    async fn read_file_with_line_range() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesReadFileTool::new(store);

        let out = tool
            .call(&json!({"path": "progress.md", "start_line": 2}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("task 2 pending"));
        assert!(!t.contains("task 1 done"));
    }

    #[tokio::test]
    async fn read_file_not_found() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesReadFileTool::new(store);

        let out = tool
            .call(&json!({"path": "nonexistent.md"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("[Error]"));
    }

    #[tokio::test]
    async fn search_finds_match() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesSearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "bug"}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("findings.md"));
        assert!(t.contains("found a bug"));
    }

    #[tokio::test]
    async fn search_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesSearchContentsTool::new(store);

        let out = tool
            .call(&json!({"query": "BUG"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("bug"));
    }

    #[tokio::test]
    async fn append_creates_and_adds() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesAppendToFileTool::new(Arc::clone(&store));

        tool.call(&json!({"path": "log.md", "text": "line 1\n"}), &ctx())
            .await
            .unwrap();
        tool.call(&json!({"path": "log.md", "text": "line 2\n"}), &ctx())
            .await
            .unwrap();

        let read_tool = NotesReadFileTool::new(store);
        let out = read_tool
            .call(&json!({"path": "log.md"}), &ctx())
            .await
            .unwrap();
        let t = text(&out);
        assert!(t.contains("line 1"));
        assert!(t.contains("line 2"));
    }

    #[tokio::test]
    async fn write_overwrites() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesWriteFileTool::new(Arc::clone(&store));

        tool.call(&json!({"path": "progress.md", "text": "overwritten"}), &ctx())
            .await
            .unwrap();

        let read_tool = NotesReadFileTool::new(store);
        let out = read_tool
            .call(&json!({"path": "progress.md"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("overwritten"));
        assert!(!text(&out).contains("task 1 done"));
    }

    #[tokio::test]
    async fn write_rejects_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesWriteFileTool::new(store);

        let out = tool
            .call(&json!({"path": "/etc/passwd", "text": "hack"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("[Error]"));
    }

    #[tokio::test]
    async fn write_rejects_dotdot() {
        let tmp = TempDir::new().unwrap();
        let store = setup_store(&tmp);
        let tool = NotesWriteFileTool::new(store);

        let out = tool
            .call(&json!({"path": "../escape", "text": "hack"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("[Error]"));
    }

    #[test]
    fn create_notes_tools_returns_five() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(NotesStore::new(tmp.path(), "test", "main"));
        let tools = create_notes_tools(store);
        assert_eq!(tools.len(), 5);
    }
}
