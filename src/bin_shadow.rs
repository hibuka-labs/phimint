//! PATH shadow detection: more than one `phimint` executable on PATH.
//!
//! The installer, `phimint update` and dev installs can land in different
//! directories; PATH order then decides which binary `phimint` runs. Session
//! 20261018_7844215b hit the classic form: 0.5.0 installed into `~/.local/bin`,
//! but a stale 0.4.1 in `~/.cargo/bin` (earlier on PATH) kept winning and
//! `phimint --version` contradicted the installer. One notice at startup makes
//! the mix-up visible instead of mysterious.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Executable name for the current platform (`phimint` / `phimint.exe`).
fn exe_name() -> String {
    format!("phimint{}", std::env::consts::EXE_SUFFIX)
}

/// Every `phimint` executable found in `path_var`, in resolution order,
/// deduped by physical location (symlinked shims are one binary, not shadows).
pub fn candidates_in(path_var: &OsStr) -> Vec<PathBuf> {
    let name = exe_name();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in std::env::split_paths(path_var) {
        let cand = dir.join(&name);
        if !cand.is_file() {
            continue;
        }
        let key = std::fs::canonicalize(&cand).unwrap_or_else(|_| cand.clone());
        if seen.insert(key) {
            out.push(cand);
        }
    }
    out
}

/// Startup notice when `candidates` holds more than one binary; `None` when
/// there is nothing to warn about. `active` is the running executable.
pub fn shadow_notice(candidates: &[PathBuf], active: Option<&Path>) -> Option<String> {
    if candidates.len() < 2 {
        return None;
    }
    let active_key = active.and_then(|p| std::fs::canonicalize(p).ok());
    let list: Vec<String> = candidates
        .iter()
        .map(|c| {
            let key = std::fs::canonicalize(c).unwrap_or_else(|_| c.clone());
            let mark = if active_key.as_deref() == Some(key.as_path()) {
                " (running)"
            } else {
                ""
            };
            format!("{}{mark}", c.display())
        })
        .collect();
    Some(format!(
        "multiple phimint binaries on PATH - `phimint` runs the first match: \
         {} - remove the stale one to avoid version mix-ups",
        list.join(", ")
    ))
}

/// Notice for the current process environment, if PATH holds shadows.
pub fn notice() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let candidates = candidates_in(&path);
    shadow_notice(&candidates, std::env::current_exe().ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_exe(dir: &Path) -> PathBuf {
        let p = dir.join(exe_name());
        std::fs::write(&p, b"stub").unwrap();
        p
    }

    #[test]
    fn single_candidate_has_no_notice() {
        let dir = tempfile::tempdir().unwrap();
        let cand = write_exe(dir.path());
        assert_eq!(shadow_notice(&[cand], None), None);
    }

    #[test]
    fn two_candidates_warn_and_mark_active() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = write_exe(a.path());
        let second = write_exe(b.path());
        let msg = shadow_notice(&[first.clone(), second.clone()], Some(&second)).unwrap();
        assert!(msg.contains("first match"), "action missing: {msg}");
        assert!(
            msg.contains(&first.display().to_string()),
            "stale listed: {msg}"
        );
        assert!(
            msg.contains(&format!("{} (running)", second.display())),
            "active unmarked: {msg}"
        );
    }

    #[test]
    fn symlinked_duplicates_are_not_shadows() {
        let dir = tempfile::tempdir().unwrap();
        let real = write_exe(dir.path());
        let link_dir = dir.path().join("link-dir");
        std::fs::create_dir(&link_dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, link_dir.join(exe_name())).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&real, link_dir.join(exe_name())).unwrap();

        let path_var = std::env::join_paths([dir.path().to_path_buf(), link_dir.clone()]).unwrap();
        assert_eq!(candidates_in(&path_var).len(), 1, "symlink counted twice");
    }

    #[test]
    fn resolution_order_follows_path_order() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let first = write_exe(a.path());
        let second = write_exe(b.path());
        let path_var =
            std::env::join_paths([b.path().to_path_buf(), a.path().to_path_buf()]).unwrap();
        let found = candidates_in(&path_var);
        assert_eq!(found, vec![second, first]);
    }
}
