//! Workspace snapshot + change attribution (Phase 4 shared infrastructure).
//!
//! `decompose` and `merge` need to agree on "what changed since we split the
//! work". The framework's multi-agent runtime gives us spawn/wait/communicate
//! but *not* change attribution — sub-agents write straight into the shared
//! filesystem, and no framework hook records which agent touched which file.
//!
//! So we build a tiny tracker: `decompose` records a snapshot of the workspace
//! (relative path → content hash) plus the slice boundaries it declared;
//! `merge` re-scans and diffs against that snapshot to produce the set of
//! `Added`/`Modified`/`Removed` files. This works on bare (non-git) workspaces,
//! which the design (§10) explicitly wants to cover.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Directory names never tracked — build output, vendored deps, and VCS state.
/// Mirrors the excludes phiforge passes to `list_files` and ripgrep.
const EXCLUDED_DIRS: &[&str] = &["target", "node_modules", ".git", ".hg", ".svn"];

/// File names never tracked — toolchain-generated, not agent edits. `Cargo.lock`
/// is produced by any `cargo check`/`build` a sub-agent runs to self-verify;
/// flagging it as an out-of-scope "conflict" in `merge` is pure noise.
const EXCLUDED_FILES: &[&str] = &["Cargo.lock"];

/// One tracked file: its workspace-relative path and a content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: String,
    pub hash: u64,
}

/// Kind of change between two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// A single file-level change between the decompose snapshot and merge time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

/// One declared work slice (produced by `decompose`, consumed by `merge`).
///
/// `files` is the slice's *declared* file boundary — the files it is expected to
/// touch. `merge` uses it for conflict detection (overlap between slices, and
/// edits landing outside any slice). `context` is the minimal orientation passed
/// to the sub-agent (problem #2: context passing); `task` is the concrete ask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slice {
    pub name: String,
    pub files: Vec<String>,
    pub context: String,
    pub task: String,
}

/// Shared state between the `decompose` and `merge` tools.
#[derive(Debug, Default)]
pub struct TrackerState {
    /// Workspace snapshot recorded at decompose time.
    pub snapshot: Option<Vec<FileRecord>>,
    /// Declared slices from the last decomposition.
    pub slices: Vec<Slice>,
}

/// `Arc<Mutex<…>>`-shared holder so `decompose` and `merge` — two separate tools
/// invoked at different points of the same turn — see the same state.
#[derive(Debug, Default)]
pub struct WorkspaceTracker {
    state: Mutex<TrackerState>,
}

impl WorkspaceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the current workspace state and the declared slices.
    pub fn record(&self, root: &Path, slices: Vec<Slice>) {
        let mut state = self.state.lock().unwrap();
        state.snapshot = Some(snapshot(root));
        state.slices = slices;
    }

    /// Diff the current workspace against the recorded snapshot.
    ///
    /// Returns `None` when no snapshot was recorded (i.e. `decompose` hasn't run),
    /// so `merge` can tell the agent to decompose first.
    pub fn changed_files(&self, root: &Path) -> Option<Vec<Change>> {
        let state = self.state.lock().unwrap();
        let before = state.snapshot.as_ref()?;
        Some(diff(before, &snapshot(root)))
    }

    /// The declared slices from the last decomposition.
    pub fn slices(&self) -> Vec<Slice> {
        self.state.lock().unwrap().slices.clone()
    }
}

/// Walk the workspace and hash every tracked file.
///
/// Pure and testable: skips excluded dirs and hidden entries, hashes regular
/// files by content (not mtime — content is what matters for change detection).
pub fn snapshot(root: &Path) -> Vec<FileRecord> {
    let mut records = Vec::new();
    walk(root, root, &mut records);
    records.sort_by(|a, b| a.path.cmp(&b.path));
    records
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<FileRecord>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        // Skip hidden entries (dotfiles, `.git`, …), build-output dirs, and
        // toolchain-generated files (Cargo.lock) that aren't agent edits.
        if name.starts_with('.')
            || EXCLUDED_DIRS.contains(&name.as_ref())
            || EXCLUDED_FILES.contains(&name.as_ref())
        {
            continue;
        }

        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            if let Some(hash) = hash_file(&path) {
                out.push(FileRecord {
                    path: rel.to_string_lossy().into_owned(),
                    hash,
                });
            }
        }
        // Symlinks and special files are ignored (avoid cycles / nondeterminism).
    }

    for sub in subdirs {
        walk(root, &sub, out);
    }
}

/// Hash a file's contents (deterministic, used only to compare before/after).
fn hash_file(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Diff two snapshots into per-file changes.
///
/// Assumes both are sorted by path (as produced by [`snapshot`]). A file present
/// with a different hash is `Modified`; present only in `after` is `Added`;
/// present only in `before` is `Removed`.
pub fn diff(before: &[FileRecord], after: &[FileRecord]) -> Vec<Change> {
    let before_map: HashMap<&str, u64> = before.iter().map(|r| (r.path.as_str(), r.hash)).collect();
    let after_map: HashMap<&str, u64> = after.iter().map(|r| (r.path.as_str(), r.hash)).collect();

    let mut paths: HashSet<&str> = HashSet::new();
    paths.extend(before_map.keys().copied());
    paths.extend(after_map.keys().copied());

    let mut changes: Vec<Change> = paths
        .into_iter()
        .filter_map(|path| match (before_map.get(path), after_map.get(path)) {
            (None, Some(_)) => Some(Change {
                path: path.to_string(),
                kind: ChangeKind::Added,
            }),
            (Some(_), None) => Some(Change {
                path: path.to_string(),
                kind: ChangeKind::Removed,
            }),
            (Some(b), Some(a)) if b != a => Some(Change {
                path: path.to_string(),
                kind: ChangeKind::Modified,
            }),
            _ => None,
        })
        .collect();

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// Normalize a possibly-`./`-prefixed or backslash path to a canonical relative
/// form, so slice-declared files compare cleanly against snapshot paths.
pub fn normalize_path(p: &str) -> String {
    // Walk components and drop `CurDir` (a leading `./` is noise — `PathBuf`'s
    // `FromIterator<Component>` impl *keeps* it as a literal `.`), then convert
    // any backslashes to forward slashes so slice-declared paths compare cleanly
    // against snapshot paths.
    let mut buf = PathBuf::new();
    for comp in Path::new(p.trim()).components() {
        if let Component::CurDir = comp {
            continue;
        }
        buf.push(comp.as_os_str());
    }
    buf.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(path: &str, hash: u64) -> FileRecord {
        FileRecord {
            path: path.to_string(),
            hash,
        }
    }

    #[test]
    fn snapshot_skips_excluded_and_hidden() {
        let dir = std::env::temp_dir().join(format!("phiforge-snap-{}", std::process::id()));
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        std::fs::write(src.join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(src.join(".hidden.rs"), "secret").unwrap();
        std::fs::write(dir.join("target").join("junk"), "junk").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();

        let snap = snapshot(&dir);
        let paths: Vec<&str> = snap.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"src/a.rs"), "{paths:?}");
        assert!(paths.contains(&"Cargo.toml"), "{paths:?}");
        assert!(!paths.contains(&"src/.hidden.rs"), "hidden file tracked: {paths:?}");
        assert!(!paths.iter().any(|p| p.contains("target")), "target tracked: {paths:?}");
        assert!(!paths.iter().any(|p| p.contains("node_modules")), "node_modules tracked: {paths:?}");
        assert!(!paths.iter().any(|p| p.contains(".git")), ".git tracked: {paths:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_skips_cargo_lock() {
        // `cargo check`/`build` (run by sub-agents to self-verify) generates
        // Cargo.lock; it must not be tracked, or `merge` flags it out-of-scope.
        let dir = std::env::temp_dir().join(format!("phiforge-snap-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.lock"), "[[package]]").unwrap();
        std::fs::write(dir.join("src.rs"), "fn main() {}").unwrap();

        let snap = snapshot(&dir);
        let paths: Vec<&str> = snap.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"src.rs"), "{paths:?}");
        assert!(!paths.contains(&"Cargo.lock"), "Cargo.lock tracked: {paths:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_is_sorted_and_deterministic() {
        let dir = std::env::temp_dir().join(format!("phiforge-snap-sort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.rs"), "b").unwrap();
        std::fs::write(dir.join("a.rs"), "a").unwrap();

        let s1 = snapshot(&dir);
        let s2 = snapshot(&dir);
        assert_eq!(s1, s2);
        assert_eq!(s1[0].path, "a.rs");
        assert_eq!(s1[1].path, "b.rs");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diff_reports_all_three_kinds() {
        let before = vec![rec("a.rs", 1), rec("b.rs", 2), rec("c.rs", 3)];
        // a unchanged, b modified, c removed, d added
        let after = vec![rec("a.rs", 1), rec("b.rs", 99), rec("d.rs", 4)];

        let changes = diff(&before, &after);
        let by_path: HashMap<&str, ChangeKind> = changes
            .iter()
            .map(|c| (c.path.as_str(), c.kind))
            .collect();

        assert_eq!(changes.len(), 3, "{changes:?}");
        assert_eq!(by_path.get("b.rs"), Some(&ChangeKind::Modified));
        assert_eq!(by_path.get("c.rs"), Some(&ChangeKind::Removed));
        assert_eq!(by_path.get("d.rs"), Some(&ChangeKind::Added));
        assert!(!by_path.contains_key("a.rs"), "unchanged file reported: {changes:?}");
    }

    #[test]
    fn diff_empty_when_identical() {
        let before = vec![rec("a.rs", 1)];
        let after = vec![rec("a.rs", 1)];
        assert!(diff(&before, &after).is_empty());
    }

    #[test]
    fn normalize_path_collapses_dot_and_backslashes() {
        assert_eq!(normalize_path("./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("src\\a.rs"), "src/a.rs");
        assert_eq!(normalize_path("  src/a.rs  "), "src/a.rs");
    }

    #[test]
    fn tracker_returns_none_without_snapshot() {
        let t = WorkspaceTracker::new();
        assert!(t.changed_files(Path::new(".")).is_none());
        assert!(t.slices().is_empty());
    }
}
