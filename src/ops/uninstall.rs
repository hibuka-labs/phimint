//! Uninstall: one command out, no leftovers.
//!
//! `phimint uninstall` is the exit counterpart to the four install channels.
//! Package managers already own removing the binary (`brew uninstall`,
//! `npm uninstall -g`, `cargo uninstall`), so this module delegates that half
//! and does the two things no package manager can see:
//!
//! 1. the `~/.phimint/` data tree — `config.json` holds the API key, and
//!    leaving it behind is a hygiene problem, not a disk-space one;
//! 2. the Windows user-PATH entry `install.ps1` appends to make the binary
//!    reachable (`install.ps1:69-73`) — nothing else will ever remove it;
//! 3. the marked PATH block `install.sh` writes into the shell rc file when
//!    the binary lands outside PATH — a leftover `export PATH=` line outlives
//!    the binary it was written for.
//!
//! Everything destructive is listed and confirmed before anything is touched.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::update::install_source::{self, InstallSource};

/// Name of the data tree under the home directory.
pub const DATA_DIR_NAME: &str = ".phimint";

/// Where phimint keeps config, state, sessions, history and notes.
pub fn data_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(DATA_DIR_NAME))
}

/// Recursive size in bytes. Symlinks are skipped so a stray link cannot make
/// this loop, and unreadable entries count as zero rather than failing.
pub fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| dir_size(&entry.path()))
        .sum()
}

/// Drop `dir` from a PATH-style variable.
///
/// `sep` is `;` on Windows and `:` elsewhere. Comparison ignores case and
/// trailing separators, which is how `install.ps1` records the entry it added.
/// Entries that are not `dir` are preserved verbatim — this never "tidies" the
/// rest of the user's PATH, because a corrupted PATH is worse than no uninstall.
pub fn strip_path_entry(path_var: &str, dir: &str, sep: char) -> String {
    let normalize = |s: &str| s.trim().trim_end_matches(['/', '\\']).to_lowercase();
    let want = normalize(dir);
    if want.is_empty() {
        return path_var.to_string();
    }
    let kept: Vec<&str> = path_var
        .split(sep)
        .filter(|entry| normalize(entry) != want)
        .collect();
    kept.join(&sep.to_string())
}

/// Opening line of the block `install.sh` writes into the user's rc file.
pub const MARKER_START: &str = "# >>> phimint >>>";

/// Closing line of that block.
pub const MARKER_END: &str = "# <<< phimint <<<";

/// Remove the installer's marker block from an rc file's content.
///
/// Returns the new content, or `None` when there is no block to remove — the
/// caller then leaves the file untouched. Everything outside the block is
/// copied byte for byte: the rc file belongs to the user.
///
/// A marker without its closing line is left alone. The block is the
/// installer's, but the lines after an unterminated marker are the user's,
/// and deleting them would be a data-loss bug in a tool whose job is to
/// clean up after itself.
pub fn strip_marker_block(content: &str) -> Option<String> {
    let segments: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = String::with_capacity(content.len());
    let mut idx = 0;
    let mut changed = false;

    while idx < segments.len() {
        let segment = segments[idx];
        if !marker_line(segment, MARKER_START) {
            out.push_str(segment);
            idx += 1;
            continue;
        }

        // Find the closing marker; without it the block is not a block.
        let closing = segments[idx + 1..]
            .iter()
            .position(|later| marker_line(later, MARKER_END));
        let Some(offset) = closing else {
            out.push_str(segment);
            idx += 1;
            continue;
        };

        // The installer puts a blank line in front of the block; it goes
        // with it, so the file does not keep a hole where the block was.
        if out.ends_with("\n\n") {
            out.pop();
        } else if out == "\n" {
            out.clear();
        }
        changed = true;
        idx += 1 + offset + 1;
    }

    if changed { Some(out) } else { None }
}

/// Is this line the given marker (ignoring the line ending and trailing
/// spaces)?
fn marker_line(segment: &str, marker: &str) -> bool {
    segment.trim_end_matches(['\n', '\r']).trim_end() == marker
}

/// Remove the marker block from an rc file on disk.
///
/// Returns whether the file was changed.
pub fn strip_marker_block_file(path: &Path) -> Result<bool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let Some(stripped) = strip_marker_block(&content) else {
        return Ok(false);
    };
    std::fs::write(path, stripped)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

/// The rc files `install.sh` may have written a marker block into — one per
/// shell it supports, plus the fallback profile.
pub fn rc_candidates(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".zshrc"),
        home.join(".bashrc"),
        home.join(".profile"),
        home.join(".config").join("fish").join("config.fish"),
    ]
}

/// The rc files under `home` that actually carry the marker block.
pub fn rc_files_with_blocks(home: &Path) -> Vec<PathBuf> {
    rc_candidates(home)
        .into_iter()
        .filter(|path| {
            std::fs::read_to_string(path)
                .map(|content| content.lines().any(|line| marker_line(line, MARKER_START)))
                .unwrap_or(false)
        })
        .collect()
}

/// Human-readable byte count: `61M`, `1.2K`, `0`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

/// Everything `phimint uninstall` is about to do, gathered before the prompt.
#[derive(Debug)]
pub struct Plan {
    pub source: InstallSource,
    /// Package-manager command that removes the binary. `None` for standalone,
    /// which owns its file and deletes it directly.
    pub binary_command: Option<String>,
    /// Path of the running executable (what standalone deletes).
    pub binary_path: PathBuf,
    pub data_dir: PathBuf,
    pub data_bytes: u64,
    pub remove_data: bool,
    /// Windows only: user-PATH entry `install.ps1` added, if any.
    pub path_entry: Option<String>,
    /// Unix only: rc files carrying the installer's marker block.
    pub rc_blocks: Vec<PathBuf>,
}

/// Build the plan. Reads state only — nothing is removed here.
pub fn plan(remove_data: bool) -> Result<Plan> {
    let source = install_source::detect();
    let binary_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("phimint"));
    let data = data_dir().unwrap_or_else(|| PathBuf::from(DATA_DIR_NAME));
    let data_bytes = if data.exists() { dir_size(&data) } else { 0 };

    // `install.ps1` puts the binary in $InstallDir and appends that same
    // directory to the user PATH, so the executable's parent is the entry to
    // strip. Only standalone installs go through that path.
    let path_entry = if cfg!(windows) && !source.is_managed() {
        binary_path.parent().map(|p| p.display().to_string())
    } else {
        None
    };

    let binary_command = if source.is_managed() {
        Some(source.uninstall_command().to_string())
    } else {
        None
    };

    // The rc files the installer wrote a PATH block into. Only standalone
    // installs carry one — a package manager never edits an rc file.
    let rc_blocks = if source.is_managed() {
        Vec::new()
    } else {
        dirs::home_dir()
            .map(|home| rc_files_with_blocks(&home))
            .unwrap_or_default()
    };

    Ok(Plan {
        source,
        binary_command,
        binary_path,
        data_dir: data,
        data_bytes,
        remove_data,
        path_entry,
        rc_blocks,
    })
}

impl Plan {
    /// The prompt body: what runs where, and how much data goes with it.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        let channel = match self.source {
            InstallSource::Brew => "Homebrew",
            InstallSource::Npm => "npm",
            InstallSource::Cargo => "cargo",
            InstallSource::Standalone => "the standalone installer",
        };
        out.push_str(&format!(
            "Detected install source: {channel}\n\nThis will:\n"
        ));

        match &self.binary_command {
            Some(cmd) => out.push_str(&format!("  1. remove the binary      — `{cmd}`\n")),
            None => out.push_str(&format!(
                "  1. remove the binary      — {}\n",
                self.binary_path.display()
            )),
        }

        if self.remove_data {
            out.push_str(&format!(
                "  2. delete {}          — {} (config.json has your API key)\n",
                self.data_dir.display(),
                format_bytes(self.data_bytes)
            ));
        } else {
            out.push_str(&format!(
                "  2. keep {}        — {} (--keep-data)\n",
                self.data_dir.display(),
                format_bytes(self.data_bytes)
            ));
        }

        match &self.path_entry {
            Some(dir) => out.push_str(&format!("  3. drop this from user PATH — {dir}\n")),
            None => out.push_str("  3. (no PATH entry to clean)\n"),
        }

        for (step, rc) in self.rc_blocks.iter().enumerate() {
            out.push_str(&format!(
                "  {}. remove the PATH block from {}\n",
                step + 4,
                rc.display()
            ));
        }

        out
    }
}

/// Ask a yes/no question on stdin. Defaults to yes when the flag is set.
///
/// Destructive, so a non-interactive stdin never counts as consent: a pipe or
/// a closed stdin used to read back an empty string, which the `[Y/n]` prompt
/// would have taken as "yes". Without a human at the terminal the only way
/// through is an explicit `--yes`.
fn confirm(assume_yes: bool) -> Result<bool> {
    use std::io::{IsTerminal, Write as _};

    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() {
        println!(
            "Refusing to uninstall without confirmation: stdin is not a terminal.\n\
             Re-run with --yes to proceed."
        );
        return Ok(false);
    }

    print!("\nContinue? [Y/n] ");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Carry out a confirmed plan.
pub fn run(plan: &Plan, assume_yes: bool) -> Result<()> {
    if !confirm(assume_yes)? {
        println!("Aborted. Nothing was changed.");
        return Ok(());
    }

    // 1. Binary.
    match &plan.binary_command {
        Some(cmd) => remove_via_manager(cmd)?,
        None => remove_self(&plan.binary_path)?,
    }

    // 2. Data.
    if plan.remove_data && plan.data_dir.exists() {
        std::fs::remove_dir_all(&plan.data_dir)
            .with_context(|| format!("failed to delete {}", plan.data_dir.display()))?;
        println!("Deleted {}", plan.data_dir.display());
    } else if plan.data_dir.exists() {
        println!("Kept {}", plan.data_dir.display());
    }

    // 3. Windows user PATH.
    if let Some(dir) = &plan.path_entry {
        strip_user_path(dir)?;
    }

    // 4. The marker block the installer put into the rc file.
    for rc in &plan.rc_blocks {
        if strip_marker_block_file(rc)? {
            println!("Removed the phimint PATH block from {}", rc.display());
        }
    }

    println!("\nphimint is uninstalled.");
    Ok(())
}

/// Hand the binary to the package manager that owns it.
fn remove_via_manager(command: &str) -> Result<()> {
    let mut parts = command.split_whitespace();
    let program = parts.next().context("empty uninstall command")?;
    let args: Vec<&str> = parts.collect();

    println!("Running `{command}` ...");
    let status = Command::new(program)
        .args(&args)
        .status()
        .with_context(|| format!("failed to run `{command}`"))?;
    if !status.success() {
        anyhow::bail!("`{command}` exited with {status}. Run it yourself to finish.");
    }
    Ok(())
}

/// Delete the running executable.
///
/// Unix allows unlinking a file that is still executing. Windows does not, so
/// there we hand the delete to a detached `cmd` that waits for this process to
/// exit and then removes the file — the only way to leave nothing behind.
fn remove_self(exe: &Path) -> Result<()> {
    if !exe.exists() {
        println!("Binary already gone: {}", exe.display());
        return Ok(());
    }
    remove_self_inner(exe)
}

#[cfg(not(windows))]
fn remove_self_inner(exe: &Path) -> Result<()> {
    std::fs::remove_file(exe).with_context(|| format!("failed to delete {}", exe.display()))?;
    println!("Removed {}", exe.display());
    Ok(())
}

#[cfg(windows)]
fn remove_self_inner(exe: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: the helper gets no console window.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let spec = format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", exe.display());
    Command::new("cmd")
        .args(["/C", &spec])
        .creation_flags(DETACHED_PROCESS)
        .spawn()
        .context("failed to schedule binary deletion")?;
    println!("Removing {} once this process exits.", exe.display());
    Ok(())
}

/// Remove one directory from the Windows user PATH.
///
/// Reads and writes through the same `[Environment]` API `install.ps1` uses, so
/// the entry we drop is exactly the entry it added. A PowerShell failure leaves
/// PATH untouched and prints the command to run by hand.
#[cfg(windows)]
fn strip_user_path(dir: &str) -> Result<()> {
    let read = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "[Environment]::GetEnvironmentVariable('Path','User')",
        ])
        .output()
        .context("failed to read user PATH")?;
    let current = String::from_utf8_lossy(&read.stdout).trim().to_string();

    let updated = strip_path_entry(&current, dir, ';');
    if updated == current {
        println!("No PATH entry to remove for {dir}");
        return Ok(());
    }

    let write = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("[Environment]::SetEnvironmentVariable('Path',{updated:?},'User')"),
        ])
        .status()
        .context("failed to write user PATH")?;
    if !write.success() {
        println!(
            "Could not update user PATH. Remove {dir} from it yourself:\n  \
             [Environment]::SetEnvironmentVariable('Path',\n    \
             ([Environment]::GetEnvironmentVariable('Path','User') -split ';' | Where-Object {{ $_ -ne '{dir}' }}) -join ';',\n    \
             'User')"
        );
        return Ok(());
    }
    println!("Removed {dir} from user PATH (restart your shell)");
    Ok(())
}

#[cfg(not(windows))]
fn strip_user_path(dir: &str) -> Result<()> {
    // install.sh never touches PATH; this is a no-op on Unix.
    println!("(no PATH entry to clean for {dir})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
