//! `@` mention path picker (Phase 10).
//!
//! Typing `@` in the composer opens a file-browser popup that lets the user
//! select a file or folder by typing a path prefix (`src/`, `../../demo/`, an
//! absolute path, …) and/or arrowing to an entry. The chosen path is inserted
//! as *plain text* — workspace-relative when inside the workspace, absolute
//! otherwise — and the LLM then reads it with the existing `read_file` /
//! `repo_map` / `list_files` / `search_content` tools. There is no content
//! injection.
//!
//! All filesystem logic lives here, isolated from the ratatui state so it can
//! be unit-tested without a terminal.

use std::path::{Path, PathBuf};

/// Max entries listed per directory (bounds the popup height and `read_dir` cost).
pub const MAX_ENTRIES: usize = 200;

/// One row in the picker popup: a real filesystem entry, or the synthetic
/// "use what I typed" row (always first).
#[derive(Debug, Clone)]
pub struct Entry {
    /// Display name: the entry's basename, or the resolved typed path (synthetic).
    pub name: String,
    /// Absolute path this entry represents (the insertion target).
    pub path: PathBuf,
    pub is_dir: bool,
    /// True for the synthetic "use what I typed" row.
    pub synthetic: bool,
}

/// Split a typed `@`-prefix into the directory to list and the partial name to
/// filter on.
///
/// `../` navigates up (possibly outside the workspace); absolute prefixes are
/// used as-is; the directory is canonicalized when it exists so `..`/`.`/symlinks
/// resolve deterministically.
pub fn split_prefix(root: &Path, prefix: &str) -> (PathBuf, String) {
    match prefix.rfind('/') {
        Some(idx) => {
            let head = &prefix[..=idx]; // includes the trailing '/'
            let tail = prefix[idx + 1..].to_string();
            (resolve_dir(root, head), tail)
        }
        None => (
            root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            prefix.to_string(),
        ),
    }
}

/// Resolve the directory part of a typed prefix (absolute, or joined to `root`),
/// canonicalizing it when it exists.
fn resolve_dir(root: &Path, head: &str) -> PathBuf {
    let p = Path::new(head);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    joined.canonicalize().unwrap_or(joined)
}

/// List `dir`'s entries — directories first, then files, each sorted
/// case-insensitively — filtered to names starting with `name`. Dotfiles are
/// hidden unless the filter itself starts with `.`.
pub fn list_entries(dir: &Path, name: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for item in rd.flatten() {
            let entry_name = item.file_name();
            let entry_name = entry_name.to_string_lossy();
            if entry_name.starts_with('.') && !name.starts_with('.') {
                continue;
            }
            if !name.is_empty() && !entry_name.starts_with(name) {
                continue;
            }
            let path = item.path();
            entries.push(Entry {
                name: entry_name.into_owned(),
                is_dir: path.is_dir(),
                path,
                synthetic: false,
            });
        }
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });
    entries.truncate(MAX_ENTRIES);
    entries
}

/// Render `full` for insertion: workspace-relative when under `root`, absolute
/// otherwise (Claude Code's model — outside-workspace paths are allowed). The
/// workspace root itself renders as `.`.
pub fn rel_or_abs(root: &Path, full: &Path) -> String {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let full_canon = canonicalize_loose(full);
    match full_canon.strip_prefix(&root_canon) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().to_string(),
        Ok(_) => ".".to_string(),
        Err(_) => full_canon.to_string_lossy().to_string(),
    }
}

/// Canonicalize `path`, resolving symlinks on its *existing* prefix while
/// keeping a non-existent tail intact. Needed so `rel_or_abs` stays correct for
/// (a) paths the write tools are about to create, and (b) roots reached through
/// symlinks like macOS `/var` → `/private/var`, where `canonicalize()` on a
/// missing path would otherwise fall back to a mismatched non-canonical form.
fn canonicalize_loose(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            missing.push(name.to_os_string());
        }
        match parent.canonicalize() {
            Ok(canon) => {
                let mut out = canon;
                for m in missing.iter().rev() {
                    out.push(m);
                }
                return out;
            }
            Err(_) => cur = parent.to_path_buf(),
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique throwaway directory under the system temp dir (phimint has no
    /// `tempfile` dev-dependency). Tagged per-test and wiped on entry so a
    /// previous interrupted run can't leak stale state.
    fn scratch(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("phimint-mention-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn touch(dir: &Path, rel: &str) -> PathBuf {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, "x").unwrap();
        p
    }

    #[test]
    fn split_prefix_empty_lists_root() {
        let root = scratch("empty");
        let (dir, name) = split_prefix(&root, "");
        assert_eq!(dir, root.canonicalize().unwrap());
        assert_eq!(name, "");
    }

    #[test]
    fn split_prefix_no_slash_is_name_filter_on_root() {
        let root = scratch("noslash");
        let (dir, name) = split_prefix(&root, "src");
        assert_eq!(dir, root.canonicalize().unwrap());
        assert_eq!(name, "src");
    }

    #[test]
    fn split_prefix_trailing_slash_descends() {
        let root = scratch("descend");
        std::fs::create_dir_all(root.join("src/utils")).unwrap();
        let (dir, name) = split_prefix(&root, "src/");
        assert_eq!(dir, root.join("src").canonicalize().unwrap());
        assert_eq!(name, "");
    }

    #[test]
    fn split_prefix_partial_name_after_slash() {
        let root = scratch("partial");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let (dir, name) = split_prefix(&root, "src/ma");
        assert_eq!(dir, root.join("src").canonicalize().unwrap());
        assert_eq!(name, "ma");
    }

    #[test]
    fn split_prefix_dotdot_no_slash_is_name_filter() {
        // `..` with no trailing `/` is the *name* filter, not navigation — the
        // `/` is what descends (see the dotdot-slash test below).
        let root = scratch("dotdot");
        let (dir, name) = split_prefix(&root, "..");
        assert_eq!(dir, root.canonicalize().unwrap());
        assert_eq!(name, "..");
    }

    #[test]
    fn split_prefix_dotdot_slash_escapes_and_descends() {
        let root = scratch("dotdotslash");
        let grandparent = root.parent().unwrap().parent().unwrap();
        let (dir, name) = split_prefix(&root, "../../");
        assert_eq!(dir, grandparent.canonicalize().unwrap());
        assert_eq!(name, "");
    }

    #[test]
    fn split_prefix_absolute_used_as_is() {
        let root = scratch("absolute");
        let outside = scratch("absolute-outside");
        let (dir, name) = split_prefix(&root, &format!("{}/", outside.display()));
        assert_eq!(dir, outside.canonicalize().unwrap());
        assert_eq!(name, "");
    }

    #[test]
    fn list_entries_dirs_first_and_hides_dotfiles() {
        let dir = scratch("list");
        touch(&dir, "z_file.txt");
        touch(&dir, "a_file.txt");
        touch(&dir, ".hidden");
        std::fs::create_dir_all(dir.join("b_dir")).unwrap();
        std::fs::create_dir_all(dir.join("a_dir")).unwrap();

        let entries = list_entries(&dir, "");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a_dir", "b_dir", "a_file.txt", "z_file.txt"]);
        assert!(entries[0].is_dir);
        assert!(!entries.iter().any(|e| e.name == ".hidden"));
    }

    #[test]
    fn list_entries_filters_by_name_prefix() {
        let dir = scratch("filter");
        touch(&dir, "main.rs");
        touch(&dir, "mod.rs");
        touch(&dir, "lib.rs");

        let entries = list_entries(&dir, "ma");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["main.rs"]);
    }

    #[test]
    fn list_entries_shows_dotfiles_when_filter_starts_with_dot() {
        let dir = scratch("dotfilter");
        touch(&dir, ".env");
        touch(&dir, "Cargo.toml");

        let entries = list_entries(&dir, ".");
        assert!(entries.iter().any(|e| e.name == ".env"));
    }

    #[test]
    fn rel_or_abs_relative_inside_absolute_outside() {
        let root = scratch("relabs");
        let inside = touch(&root, "src/lib.rs");
        assert_eq!(rel_or_abs(&root, &inside), "src/lib.rs");

        let outside = scratch("relabs-outside");
        let out = touch(&outside, "demo/codex/main.rs");
        assert_eq!(rel_or_abs(&root, &out), out.canonicalize().unwrap().to_string_lossy());
    }

    #[test]
    fn rel_or_abs_root_itself_is_dot() {
        let root = scratch("rootdot");
        assert_eq!(rel_or_abs(&root, &root), ".");
    }

    #[test]
    fn rel_or_abs_nonexistent_path_is_relative() {
        let root = scratch("nonexistent");
        // Non-existent paths can't canonicalize; they stay joined and, when
        // under the root, render as a relative path.
        assert_eq!(rel_or_abs(&root, &root.join("new/file.txt")), "new/file.txt");
    }
}
