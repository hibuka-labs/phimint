//! Notes storage for token-budget context management.
//!
//! Provides a virtual file system at `~/.phimint/notes/<session_id>/<agent_name>/`
//! where the model can store persistent scratchpad data that survives
//! context window transitions.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum file size: 1 MB (1,000,000 UTF-8 bytes).
const MAX_FILE_SIZE: usize = 1_000_000;

// ── Path safety ─────────────────────────────────────────────────────────────

/// Validate a notes path for safety.
///
/// Rejects:
/// - Absolute paths
/// - Paths containing `..`
/// - Paths containing `~`
/// - Empty paths
fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err("absolute paths are not allowed".to_string());
    }
    if path.contains("..") {
        return Err("'..' is not allowed in paths".to_string());
    }
    if path.contains('~') {
        return Err("'~' is not allowed in paths".to_string());
    }
    // Reject null bytes
    if path.contains('\0') {
        return Err("null bytes are not allowed in paths".to_string());
    }
    Ok(())
}

// ── NotesStore ──────────────────────────────────────────────────────────────

/// Manages notes storage at `~/.phimint/notes/<session_id>/<agent_name>/`.
pub struct NotesStore {
    /// Root directory for this agent's notes.
    root: PathBuf,
}

impl NotesStore {
    /// Create a new store for the given base directory, session ID, and agent name.
    ///
    /// `base_dir` is typically `~/.phimint/`.
    pub fn new(base_dir: &Path, session_id: &str, agent_name: &str) -> Self {
        Self {
            root: base_dir.join("notes").join(session_id).join(agent_name),
        }
    }

    /// Resolve and validate a relative path to an absolute path under root.
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        validate_path(path)?;
        let resolved = self.root.join(path);
        // Canonicalize to prevent symlink escapes (if path exists)
        // For new files, just check the parent
        if resolved.exists() {
            let canon = resolved
                .canonicalize()
                .map_err(|e| format!("path resolution failed: {}", e))?;
            let root_canon = self
                .root
                .canonicalize()
                .map_err(|e| format!("root resolution failed: {}", e))?;
            if !canon.starts_with(&root_canon) {
                return Err("path escapes notes directory".to_string());
            }
        }
        Ok(resolved)
    }

    /// Write content to a file, creating it if it doesn't exist.
    ///
    /// Enforces the 1 MB size limit.
    pub fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        if content.len() > MAX_FILE_SIZE {
            return Err(format!(
                "content exceeds {} byte limit (got {} bytes). Split into multiple files.",
                MAX_FILE_SIZE,
                content.len()
            ));
        }
        let full_path = self.resolve(path)?;
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create directory: {}", e))?;
        }
        fs::write(&full_path, content)
            .map_err(|e| format!("failed to write file: {}", e))?;
        Ok(())
    }

    /// Append content to an existing file (or create it).
    ///
    /// Enforces the 1 MB size limit on the resulting file.
    pub fn append_to_file(&self, path: &str, content: &str) -> Result<(), String> {
        let full_path = self.resolve(path)?;

        // Check existing size
        let existing_size = fs::metadata(&full_path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);

        if existing_size + content.len() > MAX_FILE_SIZE {
            return Err(format!(
                "appending would exceed {} byte limit (existing: {}, appending: {}). Split into multiple files.",
                MAX_FILE_SIZE,
                existing_size,
                content.len()
            ));
        }

        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create directory: {}", e))?;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&full_path)
            .map_err(|e| format!("failed to open file: {}", e))?;

        file.write_all(content.as_bytes())
            .map_err(|e| format!("failed to append: {}", e))?;
        Ok(())
    }

    /// Read a file's content, optionally with line range.
    pub fn read_file(
        &self,
        path: &str,
        start_line: Option<usize>,
        stop_line: Option<usize>,
    ) -> Result<String, String> {
        let full_path = self.resolve(path)?;
        let content = fs::read_to_string(&full_path)
            .map_err(|e| format!("failed to read file: {}", e))?;

        if start_line.is_none() && stop_line.is_none() {
            return Ok(content);
        }

        let lines: Vec<&str> = content.lines().collect();
        let start = start_line.unwrap_or(1).max(1) - 1; // 1-indexed to 0-indexed
        let end = stop_line.unwrap_or(lines.len()).min(lines.len());

        if start >= lines.len() {
            return Ok(String::new());
        }

        Ok(lines[start..end].join("\n"))
    }

    /// List files under an optional prefix.
    pub fn list_files(&self, prefix: Option<&str>, max_results: usize) -> Vec<String> {
        let search_root = match prefix {
            Some(p) => match validate_path(p) {
                Ok(()) => self.root.join(p),
                Err(_) => self.root.clone(),
            },
            None => self.root.clone(),
        };

        let mut files = Vec::new();
        self.walk_dir(&search_root, &self.root, &mut files, max_results);
        files.sort();
        files.truncate(max_results);
        files
    }

    fn walk_dir(&self, dir: &Path, root: &Path, files: &mut Vec<String>, max: usize) {
        if files.len() >= max {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            if files.len() >= max {
                return;
            }
            let path = entry.path();
            if path.is_dir() {
                self.walk_dir(&path, root, files, max);
            } else if path.is_file() {
                if let Ok(relative) = path.strip_prefix(root) {
                    if let Some(s) = relative.to_str() {
                        files.push(s.to_string());
                    }
                }
            }
        }
    }

    /// Search file contents for a substring.
    pub fn search_contents(
        &self,
        query: &str,
        prefix: Option<&str>,
        max_files: usize,
        max_matches: usize,
    ) -> Vec<NoteSearchResult> {
        let files = self.list_files(prefix, max_files);
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();

        for file_path in &files {
            let content = match fs::read_to_string(self.root.join(file_path)) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let mut file_matches = Vec::new();
            for (line_num, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&query_lower) {
                    file_matches.push(NoteMatch {
                        line_number: line_num + 1,
                        line_content: line.to_string(),
                    });
                    if file_matches.len() >= max_matches {
                        break;
                    }
                }
            }

            if !file_matches.is_empty() {
                results.push(NoteSearchResult {
                    path: file_path.clone(),
                    matches: file_matches,
                });
            }

            if results.len() >= max_files {
                break;
            }
        }

        results
    }
}

/// A matching line in a notes file.
#[derive(Clone, Debug)]
pub struct NoteMatch {
    pub line_number: usize,
    pub line_content: String,
}

/// Search result for a single notes file.
#[derive(Clone, Debug)]
pub struct NoteSearchResult {
    pub path: String,
    pub matches: Vec<NoteMatch>,
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(tmp: &TempDir) -> NotesStore {
        NotesStore::new(tmp.path(), "test_session", "main")
    }

    // ── Path safety ──

    #[test]
    fn reject_absolute_path() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("/etc/passwd", "x").is_err());
    }

    #[test]
    fn reject_dotdot() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("../escape", "x").is_err());
    }

    #[test]
    fn reject_tilde() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("~/escape", "x").is_err());
    }

    #[test]
    fn reject_empty_path() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("", "x").is_err());
    }

    #[test]
    fn reject_null_byte() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("file\0name", "x").is_err());
    }

    #[test]
    fn accept_valid_path() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("progress.md", "ok").is_ok());
    }

    #[test]
    fn accept_nested_path() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("sub/dir/file.md", "ok").is_ok());
    }

    // ── write_file + read_file ──

    #[test]
    fn write_then_read() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("note.md", "hello world").unwrap();
        assert_eq!(s.read_file("note.md", None, None).unwrap(), "hello world");
    }

    #[test]
    fn write_overwrites() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("note.md", "first").unwrap();
        s.write_file("note.md", "second").unwrap();
        assert_eq!(s.read_file("note.md", None, None).unwrap(), "second");
    }

    #[test]
    fn write_rejects_oversized() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("big.md", &"x".repeat(1_000_001)).is_err());
    }

    #[test]
    fn write_accepts_exactly_1mb() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        assert!(s.write_file("max.md", &"x".repeat(1_000_000)).is_ok());
    }

    // ── append_to_file ──

    #[test]
    fn append_creates_file() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.append_to_file("log.md", "line 1\n").unwrap();
        assert_eq!(s.read_file("log.md", None, None).unwrap(), "line 1\n");
    }

    #[test]
    fn append_adds_content() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.append_to_file("log.md", "line 1\n").unwrap();
        s.append_to_file("log.md", "line 2\n").unwrap();
        assert_eq!(
            s.read_file("log.md", None, None).unwrap(),
            "line 1\nline 2\n"
        );
    }

    #[test]
    fn append_rejects_oversized() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("big.md", &"x".repeat(999_999)).unwrap();
        assert!(s.append_to_file("big.md", "xx").is_err()); // would be 1,000,001
    }

    // ── read_file with line range ──

    #[test]
    fn read_file_line_range() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("lines.md", "a\nb\nc\nd\ne").unwrap();

        assert_eq!(s.read_file("lines.md", Some(2), Some(4)).unwrap(), "b\nc\nd");
        assert_eq!(s.read_file("lines.md", Some(1), Some(1)).unwrap(), "a");
        assert_eq!(s.read_file("lines.md", Some(5), None).unwrap(), "e");
        assert_eq!(s.read_file("lines.md", None, Some(3)).unwrap(), "a\nb\nc");
    }

    #[test]
    fn read_file_start_beyond_content() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("short.md", "a\nb").unwrap();
        assert_eq!(s.read_file("short.md", Some(10), None).unwrap(), "");
    }

    // ── list_files ──

    #[test]
    fn list_files_returns_all() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("a.md", "a").unwrap();
        s.write_file("b.md", "b").unwrap();
        s.write_file("sub/c.md", "c").unwrap();

        let files = s.list_files(None, 100);
        assert_eq!(files.len(), 3);
        assert!(files.contains(&"a.md".to_string()));
        assert!(files.contains(&"sub/c.md".to_string()));
    }

    #[test]
    fn list_files_with_prefix() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("notes/a.md", "a").unwrap();
        s.write_file("notes/b.md", "b").unwrap();
        s.write_file("other/c.md", "c").unwrap();

        let files = s.list_files(Some("notes"), 100);
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|f| f.starts_with("notes/")));
    }

    #[test]
    fn list_files_respects_limit() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("a.md", "a").unwrap();
        s.write_file("b.md", "b").unwrap();
        s.write_file("c.md", "c").unwrap();

        let files = s.list_files(None, 2);
        assert_eq!(files.len(), 2);
    }

    // ── search_contents ──

    #[test]
    fn search_finds_match() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("progress.md", "found the needle\nother line").unwrap();

        let results = s.search_contents("needle", None, 100, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "progress.md");
        assert_eq!(results[0].matches[0].line_number, 1);
        assert!(results[0].matches[0].line_content.contains("needle"));
    }

    #[test]
    fn search_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("f.md", "Hello World").unwrap();

        let results = s.search_contents("hello", None, 100, 10);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_with_prefix() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("dir1/a.md", "needle").unwrap();
        s.write_file("dir2/b.md", "needle").unwrap();

        let results = s.search_contents("needle", Some("dir1"), 100, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "dir1/a.md");
    }

    #[test]
    fn search_respects_max_files() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("a.md", "needle").unwrap();
        s.write_file("b.md", "needle").unwrap();

        let results = s.search_contents("needle", None, 1, 10);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_no_match() {
        let tmp = TempDir::new().unwrap();
        let s = store(&tmp);
        s.write_file("a.md", "nothing here").unwrap();

        let results = s.search_contents("needle", None, 100, 10);
        assert!(results.is_empty());
    }
}
