//! Which sessions the resume picker puts in front of you.
//!
//! `phi_agent::list_sessions` returns everything resumable. What to *show* is
//! product policy and lives here: the default view is the folder phimint was
//! launched from, and only recent work. `Tab` widens to everything.
//! Nothing on disk is filtered, hidden, or deleted by any of this.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use phi_agent::SessionInfo;

/// How far back the default view reaches. Older sessions are not gone —
/// [`Scope::All`] still lists them.
///
/// This is a *display* window. Disk retention is a separate knob
/// (`--session-retention-days`) and has to be at least this long, or the
/// window has nothing to show.
pub const RECENT_DAYS: u64 = 30;

/// What the picker is currently showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Sessions that started in this folder and are recent enough to matter.
    Workspace,
    /// Every resumable session: all folders, no age cut.
    All,
}

impl Scope {
    /// The other end of the `Tab` toggle.
    pub fn toggled(self) -> Self {
        match self {
            Scope::Workspace => Scope::All,
            Scope::All => Scope::Workspace,
        }
    }
}

/// The process working directory, canonicalized the same way `phi-agent`
/// records a session's birthplace so the two sides compare equal.
pub fn current_workspace() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(std::fs::canonicalize(&cwd).unwrap_or(cwd))
}

/// Sessions visible in `scope`, keeping the order `list_sessions` produced.
pub fn visible<'a>(
    entries: &'a [SessionInfo],
    scope: Scope,
    workspace: Option<&Path>,
) -> Vec<&'a SessionInfo> {
    entries
        .iter()
        .filter(|e| in_scope(e, scope, workspace))
        .collect()
}

fn in_scope(entry: &SessionInfo, scope: Scope, workspace: Option<&Path>) -> bool {
    match scope {
        Scope::All => true,
        Scope::Workspace => is_recent(entry) && is_local(entry, workspace),
    }
}

/// A session belongs to this folder when its recorded birthplace is here.
/// Sessions with no recorded birthplace have no home, so they never match —
/// [`Scope::All`] is the only place they show up.
///
/// Nothing is guessed at this layer. [`crate::session_birthplace`] may have
/// filed a home and written it down, but by the time a session reaches here
/// the value is recorded fact or it is absent.
fn is_local(entry: &SessionInfo, workspace: Option<&Path>) -> bool {
    match (&entry.cwd, workspace) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn is_recent(entry: &SessionInfo) -> bool {
    match SystemTime::now().duration_since(entry.last_active_at) {
        Ok(age) => age <= Duration::from_secs(RECENT_DAYS * 86400),
        // Clock skew: a timestamp in the future is not a reason to hide it.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cwd: Option<&str>, age_days: u64) -> SessionInfo {
        entry_secs(cwd, age_days * 86400)
    }

    fn entry_secs(cwd: Option<&str>, age_secs: u64) -> SessionInfo {
        SessionInfo {
            session_id: "s".into(),
            title: "t".into(),
            last_active_at: SystemTime::now() - Duration::from_secs(age_secs),
            session_dir: PathBuf::from("/tmp/s"),
            cwd: cwd.map(PathBuf::from),
        }
    }

    fn ws() -> Option<&'static Path> {
        Some(Path::new("/work/repo"))
    }

    #[test]
    fn workspace_scope_shows_only_this_folder() {
        let all = vec![entry(Some("/work/repo"), 1), entry(Some("/work/other"), 1)];
        let shown = visible(&all, Scope::Workspace, ws());
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].cwd, Some(PathBuf::from("/work/repo")));
    }

    /// Untagged legacy sessions have no home. Showing them everywhere would
    /// put every old conversation back into every folder — the noise this
    /// feature exists to remove.
    #[test]
    fn untagged_sessions_stay_out_of_the_workspace_view() {
        let all = vec![entry(None, 1)];
        assert!(visible(&all, Scope::Workspace, ws()).is_empty());
        assert_eq!(visible(&all, Scope::All, ws()).len(), 1);
    }

    #[test]
    fn workspace_scope_enforces_the_age_window() {
        let all = vec![entry(Some("/work/repo"), 29), entry(Some("/work/repo"), 31)];
        let shown = visible(&all, Scope::Workspace, ws());
        assert_eq!(shown.len(), 1);
        assert_eq!(
            shown[0].last_active_at, all[0].last_active_at,
            "the recent one is kept, not just any one"
        );
    }

    #[test]
    fn all_scope_ignores_folder_and_age() {
        let all = vec![
            entry(Some("/work/other"), 400),
            entry(None, 400),
            entry(Some("/work/repo"), 1),
        ];
        assert_eq!(visible(&all, Scope::All, ws()).len(), 3);
    }

    /// The cut is `age <= RECENT_DAYS`, so day 30 still counts. An entry
    /// pinned *exactly* on the boundary races the clock — the check runs a
    /// moment later and it is already past it — so this pins a few seconds
    /// on each side instead.
    #[test]
    fn the_age_window_cuts_off_just_past_recdays() {
        let edge = RECENT_DAYS * 86400;
        let all = vec![
            entry_secs(Some("/work/repo"), edge - 5),
            entry_secs(Some("/work/repo"), edge + 5),
        ];
        let shown = visible(&all, Scope::Workspace, ws());
        assert_eq!(shown.len(), 1);
        assert_eq!(
            shown[0].last_active_at, all[0].last_active_at,
            "the just-inside one"
        );
    }

    /// A timestamp ahead of the local clock is clock skew, not future work.
    #[test]
    fn a_future_timestamp_is_not_hidden() {
        let mut e = entry(Some("/work/repo"), 0);
        e.last_active_at = SystemTime::now() + Duration::from_secs(86400);
        assert_eq!(visible(&[e], Scope::Workspace, ws()).len(), 1);
    }

    #[test]
    fn without_a_known_workspace_nothing_is_local() {
        let all = vec![entry(Some("/work/repo"), 1)];
        assert!(visible(&all, Scope::Workspace, None).is_empty());
    }

    #[test]
    fn toggle_flips_both_ways() {
        assert_eq!(Scope::Workspace.toggled(), Scope::All);
        assert_eq!(Scope::All.toggled(), Scope::Workspace);
    }
}
