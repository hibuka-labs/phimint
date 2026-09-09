//! History storage for token-budget context management.
//!
//! Archives context window messages to JSONL files and maintains
//! per-session metadata. Used by `TokenBudgetCompactor` during
//! window resets.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use phi_agent::ChatMessage;

// ── Metadata ────────────────────────────────────────────────────────────────

/// Metadata for a single context window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowMeta {
    /// Window ID (zero-padded 3-digit string, e.g. "001").
    pub id: String,
    /// ISO 8601 timestamp when the window was archived.
    pub started_at: String,
    /// Number of messages archived in this window.
    pub message_count: usize,
}

/// Session-level metadata stored in `metadata.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub windows: Vec<WindowMeta>,
}

// ── ChatMessage JSONL serialization ─────────────────────────────────────────

/// A single JSONL record for an archived chat message.
#[derive(Serialize, Deserialize)]
struct MessageRecord {
    window: usize,
    index: usize,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    content: String,
    ts: String,
}

fn message_role(msg: &ChatMessage) -> String {
    match msg {
        ChatMessage::System { .. } => "system".to_string(),
        ChatMessage::User { .. } => "user".to_string(),
        ChatMessage::Assistant { .. } => "assistant".to_string(),
        ChatMessage::Tool { .. } => "tool".to_string(),
        ChatMessage::Custom { role, .. } => role.clone(),
    }
}

fn message_content(msg: &ChatMessage) -> String {
    match msg {
        ChatMessage::System { content, .. } => content.clone(),
        ChatMessage::User { content, .. } => content.clone(),
        ChatMessage::Assistant { content, .. } => {
            content.clone().unwrap_or_default()
        }
        ChatMessage::Tool { content, .. } => content.clone(),
        ChatMessage::Custom { data, .. } => data.to_string(),
    }
}

fn message_name(msg: &ChatMessage) -> Option<String> {
    match msg {
        ChatMessage::Tool { name, .. } => name.clone(),
        _ => None,
    }
}

// ── Query result types ──────────────────────────────────────────────────────

/// Summary of a message item for `list_items`.
#[derive(Clone, Debug)]
pub struct ItemSummary {
    pub window: usize,
    pub index: usize,
    pub role: String,
    pub name: Option<String>,
    pub content_preview: String,
}

/// Search result for `search_contents`.
#[derive(Clone, Debug)]
pub struct SearchResult {
    pub window: usize,
    pub index: usize,
    pub role: String,
    pub name: Option<String>,
    pub match_preview: String,
}

// ── Helper functions ────────────────────────────────────────────────────────

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        format!("{}...", &s[..max_chars])
    }
}

/// Extract a preview around the first match of `query` in `content`.
fn extract_match_preview(content: &str, query_lower: &str, context_chars: usize) -> String {
    let content_lower = content.to_lowercase();
    if let Some(pos) = content_lower.find(query_lower) {
        let start = pos.saturating_sub(context_chars);
        let end = (pos + query_lower.len() + context_chars).min(content.len());
        let snippet = &content[start..end];
        if start > 0 {
            format!("...{}...", snippet)
        } else {
            format!("{}...", snippet)
        }
    } else {
        truncate_str(content, context_chars * 2)
    }
}

// ── HistoryStore ────────────────────────────────────────────────────────────

/// Manages history storage at `~/.phimint/history/<session_id>/`.
pub struct HistoryStore {
    session_dir: PathBuf,
}

impl HistoryStore {
    /// Create a new store for the given base directory and session ID.
    ///
    /// `base_dir` is typically `~/.phimint/`. The session directory
    /// (`<base_dir>/history/<session_id>/`) is created on first write.
    pub fn new(base_dir: &Path, session_id: &str) -> Self {
        Self {
            session_dir: base_dir.join("history").join(session_id),
        }
    }

    /// Archive messages to a JSONL file for the given window.
    ///
    /// Creates `<session_dir>/windows/<window_id>.jsonl` and updates
    /// `metadata.json`. Returns the number of messages written.
    pub fn archive_window(
        &self,
        window_id: usize,
        messages: &[ChatMessage],
    ) -> std::io::Result<usize> {
        // Ensure directories exist
        let windows_dir = self.session_dir.join("windows");
        fs::create_dir_all(&windows_dir)?;

        // Write JSONL
        let file_name = format!("{:03}.jsonl", window_id);
        let file_path = windows_dir.join(&file_name);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;

        let ts = chrono::Utc::now().to_rfc3339();
        for (i, msg) in messages.iter().enumerate() {
            let record = MessageRecord {
                window: window_id,
                index: i,
                role: message_role(msg),
                name: message_name(msg),
                content: message_content(msg),
                ts: ts.clone(),
            };
            writeln!(file, "{}", serde_json::to_string(&record)?)?;
        }
        file.flush()?;

        // Update metadata
        self.update_metadata(window_id, messages.len())?;

        Ok(messages.len())
    }

    /// Read the session metadata, or create a default if it doesn't exist.
    pub fn load_metadata(&self) -> SessionMeta {
        let meta_path = self.session_dir.join("metadata.json");
        match fs::read_to_string(&meta_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| SessionMeta {
                session_id: self.session_id().to_string(),
                windows: Vec::new(),
            }),
            Err(_) => SessionMeta {
                session_id: self.session_id().to_string(),
                windows: Vec::new(),
            },
        }
    }

    fn update_metadata(&self, window_id: usize, message_count: usize) -> std::io::Result<()> {
        let mut meta = self.load_metadata();
        let window_id_str = format!("{:03}", window_id);

        // Upsert window entry
        if let Some(w) = meta.windows.iter_mut().find(|w| w.id == window_id_str) {
            w.message_count = message_count;
            w.started_at = chrono::Utc::now().to_rfc3339();
        } else {
            meta.windows.push(WindowMeta {
                id: window_id_str,
                started_at: chrono::Utc::now().to_rfc3339(),
                message_count,
            });
        }

        let meta_path = self.session_dir.join("metadata.json");
        fs::create_dir_all(&self.session_dir)?;
        fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        Ok(())
    }

    fn session_id(&self) -> &str {
        self.session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    /// List archived window IDs (sorted ascending).
    pub fn list_windows(&self) -> Vec<String> {
        let meta = self.load_metadata();
        let mut ids: Vec<String> = meta.windows.iter().map(|w| w.id.clone()).collect();
        ids.sort();
        ids
    }

    /// Read all records from a window's JSONL file.
    fn read_records(&self, window_id: usize) -> std::io::Result<Vec<MessageRecord>> {
        let file_name = format!("{:03}.jsonl", window_id);
        let path = self.session_dir.join("windows").join(&file_name);
        let content = fs::read_to_string(&path)?;
        let records: Vec<MessageRecord> = content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(records)
    }

    /// List message items in a window with optional filtering.
    ///
    /// Returns a summary of each matching message (role, name, content preview).
    pub fn list_items(
        &self,
        window_id: Option<usize>,
        role: Option<&str>,
        tool_name: Option<&str>,
        limit: usize,
        recent_first: bool,
    ) -> Vec<ItemSummary> {
        let windows = match window_id {
            Some(id) => vec![id],
            None => {
                let ids = self.list_windows();
                ids.iter()
                    .filter_map(|id| id.parse::<usize>().ok())
                    .collect()
            }
        };

        let mut items = Vec::new();
        for wid in windows {
            let records = match self.read_records(wid) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for rec in records {
                if let Some(r) = role {
                    if rec.role != r {
                        continue;
                    }
                }
                if let Some(tn) = tool_name {
                    match &rec.name {
                        Some(n) if n == tn => {}
                        _ => continue,
                    }
                }
                items.push(ItemSummary {
                    window: rec.window,
                    index: rec.index,
                    role: rec.role,
                    name: rec.name,
                    content_preview: truncate_str(&rec.content, 200),
                });
            }
        }

        if recent_first {
            items.reverse();
        }
        items.truncate(limit);
        items
    }

    /// Read a single item's content by window_id and item index.
    ///
    /// Supports optional character range (offset_chars, limit_chars).
    pub fn read_item(
        &self,
        window_id: usize,
        item_id: usize,
        offset_chars: Option<usize>,
        limit_chars: Option<usize>,
    ) -> Option<String> {
        let records = self.read_records(window_id).ok()?;
        let rec = records.into_iter().find(|r| r.index == item_id)?;

        let content = rec.content;
        let start = offset_chars.unwrap_or(0).min(content.len());
        let end = match limit_chars {
            Some(limit) => (start + limit).min(content.len()),
            None => content.len(),
        };
        Some(content[start..end].to_string())
    }

    /// Search content across windows.
    ///
    /// Returns matching items with content preview.
    pub fn search_contents(
        &self,
        query: &str,
        window_id: Option<usize>,
        role: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        let windows = match window_id {
            Some(id) => vec![id],
            None => {
                let ids = self.list_windows();
                ids.iter()
                    .filter_map(|id| id.parse::<usize>().ok())
                    .collect()
            }
        };

        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for wid in windows {
            let records = match self.read_records(wid) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for rec in records {
                if let Some(r) = role {
                    if rec.role != r {
                        continue;
                    }
                }
                if !rec.content.to_lowercase().contains(&query_lower) {
                    continue;
                }
                // Find the match context
                let preview = extract_match_preview(&rec.content, &query_lower, 100);
                results.push(SearchResult {
                    window: rec.window,
                    index: rec.index,
                    role: rec.role,
                    name: rec.name,
                    match_preview: preview,
                });
                if results.len() >= limit {
                    return results;
                }
            }
        }

        results
    }
}

// ── Thread Hint ─────────────────────────────────────────────────────────────

/// Maximum size for thread hint injection (4 KB).
const THREAD_HINT_MAX_BYTES: usize = 4096;

/// Read the thread hint from notes, if it exists and is within size limit.
///
/// Path: `~/.phimint/notes/<session_id>/<agent_name>/thread_hint.md`
pub fn read_thread_hint(base_dir: &Path, session_id: &str, agent_name: &str) -> Option<String> {
    let path = base_dir
        .join("notes")
        .join(session_id)
        .join(agent_name)
        .join("thread_hint.md");

    let content = fs::read_to_string(&path).ok()?;
    if content.len() > THREAD_HINT_MAX_BYTES {
        tracing::warn!(
            path = %path.display(),
            size = content.len(),
            max = THREAD_HINT_MAX_BYTES,
            "thread hint exceeds size limit, skipping"
        );
        return None;
    }
    Some(content)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_messages(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| match i % 3 {
                0 => ChatMessage::user(format!("user message {}", i)),
                1 => ChatMessage::assistant(format!("assistant message {}", i)),
                _ => ChatMessage::tool(format!("tc_{}", i), format!("tool output {}", i)),
            })
            .collect()
    }

    #[test]
    fn archive_creates_jsonl_file() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test_session");
        let msgs = make_messages(5);
        store.archive_window(1, &msgs).unwrap();

        let jsonl_path = tmp
            .path()
            .join("history")
            .join("test_session")
            .join("windows")
            .join("001.jsonl");
        assert!(jsonl_path.exists());
    }

    #[test]
    fn archive_jsonl_format_correct() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test_session");
        let msgs = vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi there"),
            ChatMessage::tool_with_name("tc1", "shell", "output"),
        ];
        store.archive_window(1, &msgs).unwrap();

        let jsonl_path = tmp
            .path()
            .join("history")
            .join("test_session")
            .join("windows")
            .join("001.jsonl");
        let content = fs::read_to_string(&jsonl_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);

        // Check first record
        let rec: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(rec["window"], 1);
        assert_eq!(rec["index"], 0);
        assert_eq!(rec["role"], "user");
        assert_eq!(rec["content"], "hello");
        assert!(rec["ts"].as_str().is_some());

        // Check tool record has name
        let tool_rec: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(tool_rec["role"], "tool");
        assert_eq!(tool_rec["name"], "shell");
    }

    #[test]
    fn archive_updates_metadata() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test_session");
        store.archive_window(1, &make_messages(3)).unwrap();
        store.archive_window(2, &make_messages(5)).unwrap();

        let meta = store.load_metadata();
        assert_eq!(meta.session_id, "test_session");
        assert_eq!(meta.windows.len(), 2);
        assert_eq!(meta.windows[0].id, "001");
        assert_eq!(meta.windows[0].message_count, 3);
        assert_eq!(meta.windows[1].id, "002");
        assert_eq!(meta.windows[1].message_count, 5);
    }

    #[test]
    fn archive_overwrites_existing_window() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test_session");
        store.archive_window(1, &make_messages(3)).unwrap();
        store.archive_window(1, &make_messages(7)).unwrap();

        let meta = store.load_metadata();
        assert_eq!(meta.windows.len(), 1);
        assert_eq!(meta.windows[0].message_count, 7);
    }

    #[test]
    fn list_windows_returns_sorted_ids() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test_session");
        store.archive_window(2, &make_messages(1)).unwrap();
        store.archive_window(1, &make_messages(1)).unwrap();
        store.archive_window(3, &make_messages(1)).unwrap();

        let ids = store.list_windows();
        assert_eq!(ids, vec!["001", "002", "003"]);
    }

    #[test]
    fn load_metadata_returns_default_when_missing() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "new_session");
        let meta = store.load_metadata();
        assert_eq!(meta.session_id, "new_session");
        assert!(meta.windows.is_empty());
    }

    #[test]
    fn archive_empty_messages() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test_session");
        let count = store.archive_window(1, &[]).unwrap();
        assert_eq!(count, 0);

        let meta = store.load_metadata();
        assert_eq!(meta.windows[0].message_count, 0);
    }

    #[test]
    fn read_thread_hint_returns_content() {
        let tmp = TempDir::new().unwrap();
        let hint_dir = tmp.path().join("notes").join("s1").join("agent");
        fs::create_dir_all(&hint_dir).unwrap();
        fs::write(hint_dir.join("thread_hint.md"), "previous context").unwrap();

        let hint = read_thread_hint(tmp.path(), "s1", "agent");
        assert_eq!(hint.as_deref(), Some("previous context"));
    }

    #[test]
    fn read_thread_hint_returns_none_when_missing() {
        let tmp = TempDir::new().unwrap();
        let hint = read_thread_hint(tmp.path(), "s1", "agent");
        assert!(hint.is_none());
    }

    #[test]
    fn read_thread_hint_skips_oversized() {
        let tmp = TempDir::new().unwrap();
        let hint_dir = tmp.path().join("notes").join("s1").join("agent");
        fs::create_dir_all(&hint_dir).unwrap();
        // Write 5KB (exceeds 4KB limit)
        fs::write(
            hint_dir.join("thread_hint.md"),
            "x".repeat(5000),
        )
        .unwrap();

        let hint = read_thread_hint(tmp.path(), "s1", "agent");
        assert!(hint.is_none());
    }

    #[test]
    fn read_thread_hint_accepts_exactly_4kb() {
        let tmp = TempDir::new().unwrap();
        let hint_dir = tmp.path().join("notes").join("s1").join("agent");
        fs::create_dir_all(&hint_dir).unwrap();
        fs::write(
            hint_dir.join("thread_hint.md"),
            "x".repeat(THREAD_HINT_MAX_BYTES),
        )
        .unwrap();

        let hint = read_thread_hint(tmp.path(), "s1", "agent");
        assert!(hint.is_some());
    }

    // ── list_items tests ──

    #[test]
    fn list_items_returns_all_messages() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store.archive_window(1, &make_messages(5)).unwrap();

        let items = store.list_items(None, None, None, 100, false);
        assert_eq!(items.len(), 5);
        assert_eq!(items[0].role, "user");
        assert_eq!(items[1].role, "assistant");
        assert_eq!(items[2].role, "tool");
    }

    #[test]
    fn list_items_filters_by_role() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store.archive_window(1, &make_messages(6)).unwrap();

        let users = store.list_items(None, Some("user"), None, 100, false);
        assert!(users.iter().all(|i| i.role == "user"));
        assert_eq!(users.len(), 2); // indices 0, 3
    }

    #[test]
    fn list_items_filters_by_window() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store.archive_window(1, &make_messages(3)).unwrap();
        store.archive_window(2, &make_messages(3)).unwrap();

        let items = store.list_items(Some(1), None, None, 100, false);
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|i| i.window == 1));
    }

    #[test]
    fn list_items_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store.archive_window(1, &make_messages(10)).unwrap();

        let items = store.list_items(None, None, None, 3, false);
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn list_items_recent_first() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store.archive_window(1, &make_messages(3)).unwrap();

        let items = store.list_items(None, None, None, 100, true);
        assert_eq!(items[0].index, 2); // last message first
        assert_eq!(items[2].index, 0); // first message last
    }

    // ── read_item tests ──

    #[test]
    fn read_item_returns_content() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(1, &[ChatMessage::user("hello world")])
            .unwrap();

        let content = store.read_item(1, 0, None, None).unwrap();
        assert_eq!(content, "hello world");
    }

    #[test]
    fn read_item_with_offset_and_limit() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(1, &[ChatMessage::user("hello world")])
            .unwrap();

        let content = store.read_item(1, 0, Some(6), Some(5)).unwrap();
        assert_eq!(content, "world");
    }

    #[test]
    fn read_item_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        assert!(store.read_item(99, 0, None, None).is_none());
    }

    // ── search_contents tests ──

    #[test]
    fn search_contents_finds_match() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(
                1,
                &[
                    ChatMessage::user("fix the login bug"),
                    ChatMessage::assistant("I'll investigate"),
                ],
            )
            .unwrap();

        let results = store.search_contents("login", None, None, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].window, 1);
        assert_eq!(results[0].index, 0);
        assert!(results[0].match_preview.contains("login"));
    }

    #[test]
    fn search_contents_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(1, &[ChatMessage::user("Hello World")])
            .unwrap();

        let results = store.search_contents("hello", None, None, 10);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_contents_filters_by_role() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(
                1,
                &[
                    ChatMessage::user("error in code"),
                    ChatMessage::assistant("no error here"),
                ],
            )
            .unwrap();

        let results = store.search_contents("error", None, Some("user"), 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "user");
    }

    #[test]
    fn search_contents_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(
                1,
                &[
                    ChatMessage::user("test message"),
                    ChatMessage::assistant("test again"),
                    ChatMessage::user("test third"),
                ],
            )
            .unwrap();

        let results = store.search_contents("test", None, None, 2);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_contents_across_windows() {
        let tmp = TempDir::new().unwrap();
        let store = HistoryStore::new(tmp.path(), "test");
        store
            .archive_window(1, &[ChatMessage::user("needle")])
            .unwrap();
        store
            .archive_window(2, &[ChatMessage::assistant("needle too")])
            .unwrap();

        let results = store.search_contents("needle", None, None, 10);
        assert_eq!(results.len(), 2);
    }
}
