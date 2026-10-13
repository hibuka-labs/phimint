//! Filing the home workspace of sessions created before `cwd` existed.
//!
//! `phi-agent` records where a session was born only from the moment that
//! field existed. Everything older is untagged, and a workspace-scoped resume
//! list has nowhere to put it — which reads exactly like "my history is gone".
//! It is not gone. It was never filed.
//!
//! This pass files it, once, from the only evidence a finished session leaves
//! behind: the repositories its paths touched. The rule is deliberately
//! stingy. A repo has to be the clear subject of the session, not one of
//! several it surveyed, and sessions where nothing wins stay untagged rather
//! than being pinned to whichever repo happened to come first. Being wrong
//! here puts a conversation in the wrong folder and is worse than leaving it
//! in [`crate::session_scope::Scope::All`], where it is still reachable.
//!
//! Every value this writes carries [`CwdSource::Recovered`], so a stitched
//! together home is never mistaken for one recorded at birth.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use phi_agent::{CwdSource, read_session_cwd, read_session_cwd_source, write_session_cwd};

/// A repo has to be mentioned at least this often to count as the session's
/// subject. Below this the "evidence" is a stray path in a greeting.
const MIN_HITS: usize = 3;

/// And the subject has to lead the runner-up by this factor. A session that
/// read through several repos at similar volume has no one home.
const DOMINANCE: usize = 2;

/// Longest path token worth looking at. Prose can run together with an
/// unbroken run of path-ish characters; that is not a path.
const MAX_TOKEN_LEN: usize = 512;

/// File a home workspace for every session that has none yet.
///
/// Idempotent, and cheap to call at startup: a session with a `cwd`, or one
/// an earlier pass already declined to guess about, is skipped without being
/// read. Returns `(filed, unfiled)` — how many were given a home, and how
/// many were marked as having none.
pub fn recover_missing_birthplaces(base_dir: &Path) -> (usize, usize) {
    let Some(home) = dirs::home_dir() else {
        return (0, 0);
    };
    recover_in(base_dir, &home)
}

fn recover_in(base_dir: &Path, home: &Path) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(base_dir.join("sessions")) else {
        return (0, 0);
    };

    let mut filed = 0;
    let mut unfiled = 0;
    for dir in entries.flatten() {
        let path = dir.path();
        if !path.is_dir() {
            continue;
        }
        // Hands off anything that already has a home — recorded at creation
        // or filed by an earlier pass — and anything already marked as
        // having none. The evidence in a finished session does not change.
        if read_session_cwd(&path).is_some() || read_session_cwd_source(&path).is_some() {
            continue;
        }
        match infer_workspace(&path, base_dir, home) {
            Some(ws) => {
                write_session_cwd(&path, Some(&ws), CwdSource::Recovered);
                filed += 1;
            }
            None => {
                write_session_cwd(&path, None, CwdSource::Unrecoverable);
                unfiled += 1;
            }
        }
    }
    (filed, unfiled)
}

/// The most defensible home workspace for this session, or `None` when the
/// evidence does not name one.
fn infer_workspace(session_dir: &Path, base_dir: &Path, home: &Path) -> Option<PathBuf> {
    let counts = repo_hits(session_dir, base_dir, home);
    let root = dominant_root(&counts)?;
    // Compare against the process workspace the way `session_scope` does, so
    // a symlinked launch path and its real path land in the same bucket.
    Some(std::fs::canonicalize(&root).unwrap_or(root))
}

/// The winner, when the evidence names one home unambiguously enough to file.
///
/// `counts` is most-mentioned first. A repo has to clear [`MIN_HITS`] — to be
/// the subject rather than a stray mention — and has to lead the runner-up by
/// [`DOMINANCE`].
fn dominant_root(counts: &[(PathBuf, usize)]) -> Option<PathBuf> {
    let (root, top) = counts.first()?;
    if *top < MIN_HITS {
        return None;
    }
    match counts.get(1) {
        Some((_, second)) if *top < second.saturating_mul(DOMINANCE) => None,
        _ => Some(root.clone()),
    }
}

/// Repos this session's paths touched, most-mentioned first.
///
/// Tokens are counted before they are resolved: walking up to a `.git` is a
/// chain of `stat` calls, and a busy session names the same paths over and
/// over.
fn repo_hits(session_dir: &Path, base_dir: &Path, home: &Path) -> Vec<(PathBuf, usize)> {
    let mut resolved: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut counts: BTreeMap<PathBuf, usize> = BTreeMap::new();

    for file in evidence_files(session_dir, base_dir) {
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        for token in path_tokens(&content) {
            let root = match resolved.get(token) {
                Some(root) => root.clone(),
                None => {
                    let root = repo_root_of(Path::new(token), home);
                    resolved.insert(token.to_string(), root.clone());
                    root
                }
            };
            if let Some(root) = root {
                *counts.entry(root).or_default() += 1;
            }
        }
    }

    let mut ranked: Vec<(PathBuf, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// Everything in the session that records what it touched.
///
/// `messages.jsonl` is the conversation, `turn_*.jsonl` the tool calls,
/// `session.log` the tracing log (the tool calls again, plus diagnostics —
/// it is what tips a session whose paths only show up in output), and the
/// context-rotation archive what survives of a session that rotated.
fn evidence_files(session_dir: &Path, base_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    for name in ["messages.jsonl", "session.log"] {
        let path = session_dir.join(name);
        if path.is_file() {
            files.push(path);
        }
    }

    if let Ok(rd) = std::fs::read_dir(session_dir) {
        let mut turns: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                name.starts_with("turn_") && name.ends_with(".jsonl")
            })
            .collect();
        turns.sort();
        files.extend(turns);
    }

    if let Some(session_id) = session_dir.file_name().and_then(|n| n.to_str()) {
        let windows = base_dir.join("history").join(session_id).join("windows");
        if let Ok(rd) = std::fs::read_dir(&windows) {
            let mut archived: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            archived.sort();
            files.extend(archived);
        }
    }

    files
}

/// Split prose, logs and JSON into the absolute paths they mention.
///
/// Deliberately crude — this mines finished transcripts, it does not parse a
/// format. A path ends at the first character no real path uses, which is
/// what keeps a path stuck against CJK text (`…/codex。然后`) from swallowing
/// the sentence after it.
fn path_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !is_path_char(c))
        .filter(|t| t.starts_with('/') && t.len() <= MAX_TOKEN_LEN)
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+' | '@')
}

/// The repository a path lives in: the nearest ancestor with a `.git`.
///
/// The home directory and anything above it are never accepted — a stray
/// `.git` up there would swallow every path on the machine into one bucket.
/// Paths outside home are left alone for the same reason: what a session
/// downloaded into `~/Library` is not where it was working.
fn repo_root_of(path: &Path, home: &Path) -> Option<PathBuf> {
    let mut cur = path;
    loop {
        if cur != home && !home.starts_with(cur) && cur.join(".git").exists() {
            return Some(cur.to_path_buf());
        }
        cur = cur.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn counts(pairs: &[(&str, usize)]) -> Vec<(PathBuf, usize)> {
        pairs.iter().map(|(p, n)| (PathBuf::from(*p), *n)).collect()
    }

    #[test]
    fn a_clear_subject_wins() {
        assert_eq!(
            dominant_root(&counts(&[("/work/repo", 30), ("/work/other", 5)])),
            Some(PathBuf::from("/work/repo"))
        );
    }

    /// Below the floor the "evidence" is a stray path in a greeting, not a
    /// work session with a home.
    #[test]
    fn a_handful_of_mentions_is_not_a_home() {
        assert_eq!(dominant_root(&counts(&[("/work/repo", 2)])), None);
    }

    /// The margin matters as much as the volume: a session that surveyed
    /// several repos at similar length has no single home.
    #[test]
    fn a_survey_of_several_repos_has_no_home() {
        assert_eq!(
            dominant_root(&counts(&[
                ("/work/a", 120),
                ("/work/b", 120),
                ("/work/c", 113)
            ])),
            None
        );
    }

    #[test]
    fn a_clear_lead_beats_the_runner_up() {
        assert_eq!(
            dominant_root(&counts(&[("/work/repo", 9), ("/work/other", 4)])),
            Some(PathBuf::from("/work/repo"))
        );
    }

    /// One short of the margin stays unfiled. The rule is a rule, not a vibe.
    #[test]
    fn the_margin_is_enforced_exactly() {
        assert_eq!(
            dominant_root(&counts(&[("/work/repo", 6), ("/work/other", 4)])),
            None
        );
    }

    #[test]
    fn nothing_means_no_home() {
        assert_eq!(dominant_root(&[]), None);
    }

    #[test]
    fn paths_glued_to_prose_are_split_off() {
        assert_eq!(
            path_tokens("/work/repo/src/main.rs。然后").collect::<Vec<_>>(),
            ["/work/repo/src/main.rs"]
        );
    }

    #[test]
    fn paths_survive_json_quoting() {
        assert_eq!(
            path_tokens(r#"{"file_path":"/work/repo/a.rs","n":1}"#).collect::<Vec<_>>(),
            ["/work/repo/a.rs"]
        );
    }

    /// The nearest ancestor with a `.git` is the repo — not the first
    /// directory, and not a monorepo above it.
    #[test]
    fn a_path_resolves_to_its_own_repo() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("source/proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();

        assert_eq!(repo_root_of(&repo.join("src/main.rs"), &home), Some(repo));
    }

    /// A nested repo is its own home; the outer one does not swallow it.
    #[test]
    fn a_nested_repo_wins_over_its_parent() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outer = home.join("source/outer");
        let inner = outer.join("vendor/inner");
        std::fs::create_dir_all(outer.join(".git")).unwrap();
        std::fs::create_dir_all(inner.join(".git")).unwrap();

        assert_eq!(repo_root_of(&inner.join("src/lib.rs"), &home), Some(inner));
    }

    /// A stray `.git` in the home directory would swallow every path on the
    /// machine into one bucket. It is never a workspace.
    #[test]
    fn home_itself_is_never_a_repo() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        std::fs::create_dir_all(home.join("src")).unwrap();

        assert_eq!(repo_root_of(&home.join("src/main.rs"), &home), None);
    }

    /// Downloads and chat attachments are not workspaces. A path with no repo
    /// above it simply has no home.
    #[test]
    fn a_path_with_no_repo_above_it_yields_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("Downloads")).unwrap();

        assert_eq!(repo_root_of(&home.join("Downloads/notes.md"), &home), None);
    }

    /// End to end: a session that only ever touched one repo is filed to it,
    /// and the mark says the value was recovered rather than recorded.
    #[test]
    fn a_legacy_session_is_filed_to_the_repo_it_touched() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("source/proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let file = repo.join("src/main.rs");

        let base = tmp.path().join("base");
        let sess = base.join("sessions/legacy");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session_meta.json"), r#"{"session_id":"legacy"}"#).unwrap();
        // Three mentions clears the floor.
        let line = format!("{} {} {}", file.display(), file.display(), file.display());
        std::fs::write(
            sess.join("messages.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"User":{"content": line, "images":[]}})
            ),
        )
        .unwrap();

        assert_eq!(recover_in(&base, &home), (1, 0));
        assert_eq!(
            read_session_cwd(&sess),
            Some(std::fs::canonicalize(&repo).unwrap())
        );
        assert_eq!(read_session_cwd_source(&sess), Some(CwdSource::Recovered));
    }

    /// A greeting names no repo. That is a session with no home, and it is
    /// marked as such so the next launch does not read it again.
    #[test]
    fn a_session_with_no_repo_evidence_is_marked_not_retried() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let base = tmp.path().join("base");
        let sess = base.join("sessions/hello");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session_meta.json"), r#"{"session_id":"hello"}"#).unwrap();
        std::fs::write(
            sess.join("messages.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"User":{"content":"你好","images":[]}})
            ),
        )
        .unwrap();

        assert_eq!(recover_in(&base, &home), (0, 1));
        assert_eq!(read_session_cwd(&sess), None);
        assert_eq!(
            read_session_cwd_source(&sess),
            Some(CwdSource::Unrecoverable)
        );
    }

    /// Comparing two repos is not living in either one.
    #[test]
    fn a_session_spanning_two_repos_stays_unfiled() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let a = home.join("source/a");
        let b = home.join("source/b");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(b.join(".git")).unwrap();

        let base = tmp.path().join("base");
        let sess = base.join("sessions/survey");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session_meta.json"), r#"{"session_id":"survey"}"#).unwrap();
        let mut line = String::new();
        for _ in 0..5 {
            line.push_str(&format!("{}/x.rs {}/y.rs ", a.display(), b.display()));
        }
        std::fs::write(
            sess.join("messages.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"User":{"content": line, "images":[]}})
            ),
        )
        .unwrap();

        assert_eq!(recover_in(&base, &home), (0, 1));
        assert_eq!(read_session_cwd(&sess), None);
    }

    /// A second pass is a no-op in both directions: sessions filed, and
    /// sessions the first pass declined to guess about, are left alone.
    #[test]
    fn a_second_pass_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("source/proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let file = repo.join("main.rs");

        let base = tmp.path().join("base");
        let sess = base.join("sessions/legacy");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session_meta.json"), r#"{"session_id":"legacy"}"#).unwrap();
        let line = format!("{} {} {}", file.display(), file.display(), file.display());
        std::fs::write(
            sess.join("messages.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"User":{"content": line, "images":[]}})
            ),
        )
        .unwrap();

        assert_eq!(recover_in(&base, &home), (1, 0));
        assert_eq!(recover_in(&base, &home), (0, 0));
    }

    /// A home recorded at creation is authoritative: the pass never
    /// overwrites it, whatever the transcript happens to mention.
    #[test]
    fn a_recorded_birthplace_is_never_replaced() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let repo = home.join("source/proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let file = repo.join("main.rs");

        let base = tmp.path().join("base");
        let sess = base.join("sessions/already-home");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(
            sess.join("session_meta.json"),
            r#"{"session_id":"already-home","cwd":"/original/place","cwd_source":"recorded"}"#,
        )
        .unwrap();
        let line = format!("{} {} {}", file.display(), file.display(), file.display());
        std::fs::write(
            sess.join("messages.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({"User":{"content": line, "images":[]}})
            ),
        )
        .unwrap();

        assert_eq!(recover_in(&base, &home), (0, 0));
        assert_eq!(
            read_session_cwd(&sess),
            Some(PathBuf::from("/original/place"))
        );
    }
}
