//! Tests for `phimint uninstall`.
//!
//! Split out of `uninstall.rs` under the 1000-line ceiling. The fixtures here
//! drive the real `run()` path (a fixture binary plus a fixture data tree),
//! not just the helpers, because the bug this suite pins down only shows up
//! when the whole sequence runs.

use std::path::{Path, PathBuf};

use super::uninstall::*;
use crate::update::install_source::InstallSource;

// ── strip_path_entry ────────────────────────────────────────────────

#[test]
fn strip_removes_exact_entry() {
    assert_eq!(
        strip_path_entry(
            "/a/bin:/home/u/.local/bin:/b/bin",
            "/home/u/.local/bin",
            ':'
        ),
        "/a/bin:/b/bin"
    );
}

#[test]
fn strip_ignores_trailing_separator_and_case() {
    // install.ps1 records `~\.local\bin` while the exe path may differ
    // only in case or trailing backslash.
    assert_eq!(
        strip_path_entry(
            "C:\\a;C:\\Users\\u\\.local\\bin\\;C:\\b",
            "c:\\users\\U\\.local\\bin",
            ';'
        ),
        "C:\\a;C:\\b"
    );
}

#[test]
fn strip_only_matches_full_entries_not_substrings() {
    // `C:\foo` must not swallow `C:\foobar` — the bug install.ps1's
    // `-notlike "*$InstallDir*"` check has, and the one we must not copy.
    assert_eq!(
        strip_path_entry("C:\\foo;C:\\foobar", "C:\\foo", ';'),
        "C:\\foobar"
    );
}

#[test]
fn strip_preserves_everything_else_verbatim() {
    assert_eq!(
        strip_path_entry("/opt/bin:/usr/bin", "/nowhere", ':'),
        "/opt/bin:/usr/bin"
    );
}

#[test]
fn strip_keeps_unrelated_empty_slots() {
    // Never "tidy" the rest of the PATH — a corrupted PATH is worse than
    // a leftover entry.
    assert_eq!(strip_path_entry("/a::/b:/drop", "/drop", ':'), "/a::/b");
}

#[test]
fn strip_empty_dir_is_a_no_op() {
    assert_eq!(strip_path_entry("/a:/b", "", ':'), "/a:/b");
}

#[test]
fn strip_windows_separators() {
    assert_eq!(strip_path_entry("C:\\a;C:\\b", "C:\\b", ';'), "C:\\a");
}

// ── the installer's marker block ────────────────────────────────────

/// protects: the marker block the installer writes is removed from the
/// rc file, and every other line in it is kept — the uninstaller must not
/// touch the user's own configuration.
/// fails_when: the block survives the uninstall, or a line around it is
/// dropped with it.
/// no_claim: the blank line the installer put in front of the block — it
/// is cosmetic either way.
#[test]
fn the_marker_block_is_removed_and_the_rest_of_the_file_is_kept() {
    let content = "export EDITOR=vim\n\n# >>> phimint >>>\n\
         # added by phimint installer - delete this block to undo\n\
         export PATH=\"/home/u/.local/bin:$PATH\"\n\
         # <<< phimint <<<\n\nalias ll='ls -l'\n";
    let stripped = strip_marker_block(content).expect("the block is found");
    assert!(
        !stripped.contains("phimint >>>"),
        "the opening marker is gone: {stripped}"
    );
    assert!(
        !stripped.contains("<<< phimint"),
        "the closing marker is gone: {stripped}"
    );
    assert!(
        !stripped.contains(".local/bin"),
        "the PATH line is gone: {stripped}"
    );
    assert!(
        stripped.contains("export EDITOR=vim"),
        "the line before the block is kept: {stripped}"
    );
    assert!(
        stripped.contains("alias ll='ls -l'"),
        "the line after the block is kept: {stripped}"
    );
}

/// protects: an rc file without the marker block is left exactly as it
/// was — the uninstaller never rewrites a file it has no business in.
/// fails_when: the file is rewritten (or truncated) when no block is
/// there.
#[test]
fn a_file_without_the_marker_is_left_alone() {
    let content = "export EDITOR=vim\nalias ll='ls -l'\n";
    assert_eq!(strip_marker_block(content), None);
}

/// protects: a marker without its closing line does not eat the rest of
/// the file. The block is the installer's, but the lines after an
/// unterminated one are the user's.
/// fails_when: the lines after an unterminated marker are deleted.
#[test]
fn an_unterminated_marker_does_not_eat_the_rest_of_the_file() {
    let content = "# >>> phimint >>>\nexport PATH=/x\nmy own line\n";
    assert_eq!(
        strip_marker_block(content),
        None,
        "an unterminated block is left alone"
    );
}

/// protects: the block is removed from the file on disk, and the file
/// keeps everything else.
/// fails_when: the file keeps the block, or loses a line around it.
#[test]
fn the_marker_block_is_removed_from_a_real_file() {
    let dir = tempfile::tempdir().unwrap();
    let rc = dir.path().join(".zshrc");
    std::fs::write(
        &rc,
        "export EDITOR=vim\n# >>> phimint >>>\nexport PATH=\"/x:$PATH\"\n# <<< phimint <<<\n",
    )
    .unwrap();

    assert!(strip_marker_block_file(&rc).expect("the file is readable"));
    let after = std::fs::read_to_string(&rc).unwrap();
    assert!(!after.contains("phimint"), "the block is gone: {after}");
    assert!(
        after.contains("export EDITOR=vim"),
        "the rest is kept: {after}"
    );
}

/// protects: only the rc files that actually carry the block are listed
/// for removal — the uninstaller must not touch the other rc files.
/// fails_when: an rc file without a block is scheduled for rewriting.
#[test]
fn only_the_rc_files_that_carry_the_block_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".zshrc"),
        "# >>> phimint >>>\nexport PATH=\"/x:$PATH\"\n# <<< phimint <<<\n",
    )
    .unwrap();
    std::fs::write(dir.path().join(".bashrc"), "export EDITOR=vim\n").unwrap();

    let found = rc_files_with_blocks(dir.path());
    assert_eq!(found, vec![dir.path().join(".zshrc")]);
}

/// protects: the candidate rc files are the ones the installer writes —
/// zsh, bash, fish and the fallback profile.
/// fails_when: a shell the installer supports has no candidate, so its
/// block is never removed.
#[test]
fn the_candidate_rc_files_cover_every_shell_the_installer_supports() {
    let files = rc_candidates(Path::new("/home/u"));
    let names: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with(".zshrc")),
        "zsh: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with(".bashrc")),
        "bash: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with(".profile")),
        "profile: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with("config.fish")),
        "fish: {names:?}"
    );
}

// ── format_bytes ────────────────────────────────────────────────────

#[test]
fn bytes_formatting() {
    assert_eq!(format_bytes(0), "0B");
    assert_eq!(format_bytes(512), "512B");
    assert_eq!(format_bytes(2048), "2.0K");
    assert_eq!(format_bytes(61 * 1024 * 1024), "61.0M");
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0G");
}

// ── dir_size ────────────────────────────────────────────────────────

#[test]
fn size_of_missing_dir_is_zero() {
    assert_eq!(dir_size(Path::new("/nope/not/here/at/all")), 0);
}

#[test]
fn size_sums_nested_files() {
    let dir = std::env::temp_dir().join(format!("phimint-uninstall-{}", std::process::id()));
    let nested = dir.join("a/b");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(dir.join("a/one.txt"), vec![0u8; 100]).unwrap();
    std::fs::write(nested.join("two.txt"), vec![0u8; 50]).unwrap();

    assert_eq!(dir_size(&dir), 150);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[cfg(unix)]
fn size_ignores_symlinks() {
    let dir = std::env::temp_dir().join(format!("phimint-symlink-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("real.txt"), vec![0u8; 10]).unwrap();
    std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("link.txt")).unwrap();

    assert_eq!(dir_size(&dir), 10);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ── plan.describe ───────────────────────────────────────────────────

#[test]
fn describe_lists_channel_command_and_data_size() {
    let plan = Plan {
        source: InstallSource::Brew,
        binary_command: Some("brew uninstall phimint".to_string()),
        binary_path: PathBuf::from("/opt/homebrew/Cellar/phimint/0.2.0/bin/phimint"),
        data_dir: PathBuf::from("/home/u/.phimint"),
        data_bytes: 61 * 1024 * 1024,
        remove_data: true,
        path_entry: None,
        rc_blocks: Vec::new(),
        live_sessions: Vec::new(),
    };
    let text = plan.describe();
    assert!(text.contains("Homebrew"), "channel missing: {text}");
    assert!(
        text.contains("brew uninstall phimint"),
        "cmd missing: {text}"
    );
    assert!(text.contains("61.0M"), "size missing: {text}");
    assert!(text.contains("API key"), "API-key warning missing: {text}");
    assert!(text.contains("no PATH entry"), "PATH line missing: {text}");
}

#[test]
fn describe_marks_kept_data() {
    let plan = Plan {
        source: InstallSource::Cargo,
        binary_command: Some("cargo uninstall phimint".to_string()),
        binary_path: PathBuf::from("/home/u/.cargo/bin/phimint"),
        data_dir: PathBuf::from("/home/u/.phimint"),
        data_bytes: 0,
        remove_data: false,
        path_entry: Some("/home/u/.local/bin".to_string()),
        rc_blocks: Vec::new(),
        live_sessions: Vec::new(),
    };
    let text = plan.describe();
    assert!(text.contains("--keep-data"), "keep marker missing: {text}");
    assert!(
        text.contains("/home/u/.local/bin"),
        "PATH entry missing: {text}"
    );
    assert!(
        !text.contains("API key"),
        "keep-data must not warn about the key: {text}"
    );
}

// ── live sessions under the data tree ───────────────────────────────

/// protects: a session whose process still holds its lock is named
/// before anything is deleted — the 2026-11-21 uninstall died inside
/// `sessions/` while seven sessions were live, and the user was never
/// told their data was being deleted under running sessions.
/// fails_when: a locked session is missing from the list, or a dead one
/// is reported as live.
#[test]
fn a_locked_session_is_reported_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    for name in ["live", "idle", "nolock"] {
        std::fs::create_dir_all(sessions.join(name)).unwrap();
    }
    let held = std::fs::File::create(sessions.join("live/session.lock")).unwrap();
    held.try_lock().expect("the test owns this lock");
    std::fs::File::create(sessions.join("idle/session.lock")).unwrap();

    assert_eq!(live_sessions(dir.path()), vec!["live".to_string()]);
}

/// protects: the prompt body says who is running and what to do about
/// it, before the [Y/n] confirmation.
/// fails_when: the warning names no session, or offers no action.
#[test]
fn the_plan_names_live_sessions_before_the_prompt() {
    let plan = Plan {
        source: InstallSource::Standalone,
        binary_command: None,
        binary_path: PathBuf::from("/home/u/.local/bin/phimint"),
        data_dir: PathBuf::from("/home/u/.phimint"),
        data_bytes: 61 * 1024 * 1024,
        remove_data: true,
        path_entry: None,
        rc_blocks: Vec::new(),
        live_sessions: vec![
            "20261119_670730cb".to_string(),
            "20261120_84da65a9".to_string(),
        ],
    };
    let text = plan.describe();
    assert!(text.contains("still running"), "no warning: {text}");
    assert!(
        text.contains("20261119_670730cb"),
        "no session named: {text}"
    );
    assert!(text.contains("close them"), "no action given: {text}");
}

/// protects: the uninstall completes and leaves no data tree behind
/// even while live sessions are writing into it — the 2026-11-21 run
/// died with "Directory not empty (os error 66)" halfway through
/// `sessions/`, after the binary was already gone.
/// fails_when: run() returns an error, the data dir survives at its
/// canonical path, or the binary survives the failed data step.
#[test]
fn a_data_tree_with_live_writers_is_still_removed() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".phimint");
    let sessions = data.join("sessions");
    std::fs::create_dir_all(sessions.join("live")).unwrap();
    std::fs::write(data.join("config.json"), b"{}").unwrap();
    let binary = home.path().join("phimint");
    std::fs::write(&binary, b"bin").unwrap();

    // The shape of the incident: a session that stays alive for the whole
    // uninstall, holding its lock and churning entries the way a running
    // TUI writes logs, turn files and journals.
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let sessions = sessions.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let lock = std::fs::File::create(sessions.join("live/session.lock")).unwrap();
            lock.try_lock().unwrap();
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let scratch = sessions.join(format!("churn-{n}"));
                let _ = std::fs::create_dir(&scratch);
                let _ = std::fs::write(scratch.join("perf.log"), b"x");
                let _ = std::fs::remove_dir_all(&scratch);
                let _ = std::fs::write(sessions.join("live").join("perf.log"), b"x");
                n = n.wrapping_add(1);
            }
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(20));

    let plan = Plan {
        source: InstallSource::Standalone,
        binary_command: None,
        binary_path: binary.clone(),
        data_dir: data.clone(),
        data_bytes: 0,
        remove_data: true,
        path_entry: None,
        rc_blocks: Vec::new(),
        live_sessions: live_sessions(&data),
    };
    let outcome = run(&plan, true);
    stop.store(true, Ordering::Relaxed);
    churn.join().unwrap();

    outcome.expect("live writers are a warning, not an abort");
    assert!(
        !data.exists(),
        "the data tree is gone from its canonical place"
    );
    assert_binary_gone(&binary, "the binary is removed once the data step passes");
}

/// protects: a data tree whose sweep cannot finish is reported, not
/// fatal — and the binary is removed last so the failure stays retryable
/// instead of stranding the user without a command.
/// fails_when: run() aborts before the binary step, leaving the binary
/// but no way to re-run the uninstall.
#[test]
#[cfg(unix)]
fn an_unsweepable_data_tree_still_finishes_the_uninstall() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".phimint");
    let sub = data.join("immovable");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("stuck.txt"), b"x").unwrap();
    // A directory without write permission: unlinking its entries fails and
    // the sweep cannot finish, the way the racing writers stopped it in the
    // field.
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();

    let binary = home.path().join("phimint");
    std::fs::write(&binary, b"bin").unwrap();

    let plan = Plan {
        source: InstallSource::Standalone,
        binary_command: None,
        binary_path: binary.clone(),
        data_dir: data.clone(),
        data_bytes: 0,
        remove_data: true,
        path_entry: None,
        rc_blocks: Vec::new(),
        live_sessions: Vec::new(),
    };
    let outcome = run(&plan, true);

    // Hand the tempdir its permissions back so it can clean up.
    let stuck = trash_path(&data).join("immovable");
    if stuck.exists() {
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    outcome.expect("an unfinished sweep is a warning, not an abort");
    assert!(!data.exists(), "the canonical data dir is gone");
    assert!(
        !binary.exists(),
        "the binary is still removed after the warning"
    );
}

// ── the live-session gate (second confirmation) ─────────────────────

/// The binary half of the contract, platform-shaped: Unix unlinks the running
/// exe in place, so the fixture is gone; Windows cannot unlink a running
/// image, so `remove_self` schedules the delete for after this process exits
/// and there is nothing to assert yet. The data tree is gone on both, and
/// that is what these tests pin.
#[cfg(unix)]
fn assert_binary_gone(binary: &Path, what: &str) {
    assert!(!binary.exists(), "{what}");
}

#[cfg(windows)]
fn assert_binary_gone(_binary: &Path, _what: &str) {}

/// A throwaway binary and data tree for the gate tests, with the named
/// sessions carried in the plan the way `plan()` would report them.
fn gate_fixture(home: &Path, live: &[&str]) -> Plan {
    let data = home.join(".phimint");
    std::fs::create_dir_all(data.join("sessions")).unwrap();
    std::fs::write(data.join("config.json"), b"{}").unwrap();
    let binary = home.join("phimint");
    std::fs::write(&binary, b"bin").unwrap();
    Plan {
        source: InstallSource::Standalone,
        binary_command: None,
        binary_path: binary,
        data_dir: data,
        data_bytes: 0,
        remove_data: true,
        path_entry: None,
        rc_blocks: Vec::new(),
        live_sessions: live.iter().map(|s| s.to_string()).collect(),
    }
}

/// protects: deleting under live sessions takes its own confirmation, and
/// that one defaults to NO — an Enter key alone stops at the gate and
/// nothing is touched. The main prompt's default-yes must not carry over.
/// fails_when: an empty answer deletes the tree under the running sessions.
#[test]
fn the_live_gate_stops_the_uninstall_on_an_empty_answer() {
    let home = tempfile::tempdir().unwrap();
    let plan = gate_fixture(home.path(), &["20261119_670730cb"]);
    let data = plan.data_dir.clone();
    let binary = plan.binary_path.clone();

    // Main prompt: Enter (= yes). Gate: Enter (= no).
    let mut answers = std::io::Cursor::new(&b"\n\n"[..]);
    run_with(&plan, false, true, &mut answers).expect("an abort is not an error");

    assert!(data.exists(), "the data tree survives the declined gate");
    assert!(binary.exists(), "the binary survives the declined gate");
}

/// protects: the gate is a gate, not a wall — an explicit yes runs the
/// whole uninstall, with the binary removed last.
/// fails_when: answering yes leaves anything behind.
#[test]
fn the_live_gate_proceeds_on_an_explicit_yes() {
    let home = tempfile::tempdir().unwrap();
    let plan = gate_fixture(home.path(), &["20261119_670730cb"]);
    let data = plan.data_dir.clone();
    let binary = plan.binary_path.clone();

    // Main prompt: Enter (= yes). Gate: y.
    let mut answers = std::io::Cursor::new(&b"\ny\n"[..]);
    run_with(&plan, false, true, &mut answers).expect("the uninstall completes");

    assert!(!data.exists(), "the data tree is gone");
    assert_binary_gone(&binary, "the binary is gone");
}

/// protects: no live sessions, no second question — the single Enter that
/// passes the main prompt must run the whole uninstall. A spurious gate
/// would consume that line and then default to no, leaving the tree
/// behind, so this pins the gate's firing condition too.
/// fails_when: the uninstall stops after the main prompt with no live
/// sessions named in the plan.
#[test]
fn no_live_sessions_means_no_second_question() {
    let home = tempfile::tempdir().unwrap();
    let plan = gate_fixture(home.path(), &[]);
    let data = plan.data_dir.clone();
    let binary = plan.binary_path.clone();

    let mut answers = std::io::Cursor::new(&b"\n"[..]);
    run_with(&plan, false, true, &mut answers).expect("the uninstall completes");

    assert!(!data.exists(), "the data tree is gone");
    assert_binary_gone(&binary, "the binary is gone");
}

/// protects: the leftover hints name a command the user's platform actually
/// has — `rm -rf` advice is nonsense on Windows.
/// fails_when: the hint omits the path or names the other platform's command.
#[test]
fn the_manual_removal_hint_matches_the_platform() {
    let hint = rm_hint(Path::new("/home/u/.phimint"));
    assert!(hint.contains("/home/u/.phimint"), "path named: {hint}");
    #[cfg(unix)]
    assert!(hint.starts_with("rm -rf"), "unix hint: {hint}");
    #[cfg(windows)]
    assert!(hint.starts_with("rmdir /s /q"), "windows hint: {hint}");
}
