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

/// Sessions whose process is still alive: their `session.lock` is held under
/// an exclusive lock, the same liveness probe `cleanup_expired_sessions` and
/// the /resume listing use. Deleting under them is what half-killed the
/// 2026-11-21 uninstall, so they are named before the confirm prompt.
pub fn live_sessions(data_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(data_dir.join("sessions")) else {
        return Vec::new();
    };
    let mut live: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter(|entry| {
            let lock = std::fs::File::open(entry.path().join("session.lock"));
            matches!(&lock, Ok(file) if file.try_lock_shared().is_err())
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    live.sort();
    live
}

/// Delete the data tree, live writers and all.
///
/// `std::fs::remove_dir_all` deletes entries while `readdir` streams the same
/// directory; when a running session mutates the tree mid-walk, `readdir` may
/// skip entries (POSIX leaves this unspecified) and the final `rmdir` then
/// fails `ENOTEMPTY` — the 2026-11-21 uninstall died exactly there, halfway
/// through `sessions/`, with the binary already gone. Renaming the tree out of
/// the way first is one atomic step no writer can race: the product state is
/// uninstalled even when the follow-up sweep gives up, and whatever survives
/// is reported instead of aborting the run.
///
/// The rename is an optimisation, not the contract. On Windows a running
/// session holds its log file open, and renaming a directory that holds an
/// open file fails with a sharing violation — so the rename there is
/// routinely refused, and treating that refusal as fatal made the whole
/// uninstall abort (the binary still in place, the data tree still in place)
/// over a tree that could mostly be deleted. So: rename when it works, and
/// when it does not, sweep the tree where it stands and report what is left.
fn delete_data_dir(data_dir: &Path) -> Result<()> {
    let trash = trash_path(data_dir);
    let renamed = std::fs::rename(data_dir, &trash).is_ok();
    let sweep = if renamed { trash.as_path() } else { data_dir };

    if let Err(err) = std::fs::remove_dir_all(sweep) {
        if renamed {
            // Moved out of the way and not deleted: the product state is
            // uninstalled, so this is a leftover to report, not a failure.
            println!(
                "Warning: some files could not be deleted and were left in {}\n  \
                 remove them with: {}",
                sweep.display(),
                rm_hint(sweep)
            );
        } else {
            // Not moved and not deleted: the tree is still where it was, so
            // this one is a real failure — say how to finish by hand and let
            // the caller stop here (the binary is removed last, so a retry of
            // `phimint uninstall` is still possible).
            anyhow::bail!(
                "failed to delete {}: {err}\n  \
                 close any running phimint sessions, then remove it yourself: {}",
                data_dir.display(),
                rm_hint(data_dir)
            );
        }
    }

    // A still-running session writes by absolute path and can recreate the
    // tree under the user's feet; the uninstall must say so, not claim
    // "no leftovers".
    if data_dir.exists() {
        println!(
            "Warning: {} was recreated by a still-running session\n  \
             close it and run: {}",
            data_dir.display(),
            rm_hint(data_dir)
        );
        return Ok(());
    }

    println!("Deleted {}", data_dir.display());
    Ok(())
}

/// The manual-removal command the leftover hints print: `rm -rf` on Unix,
/// `rmdir /s /q` on Windows (cmd and PowerShell both understand it).
pub(crate) fn rm_hint(path: &Path) -> String {
    if cfg!(windows) {
        format!("rmdir /s /q \"{}\"", path.display())
    } else {
        format!("rm -rf {}", path.display())
    }
}

/// Sibling directory the data tree is renamed into before sweeping.
pub(crate) fn trash_path(data_dir: &Path) -> PathBuf {
    data_dir.with_file_name(format!("{DATA_DIR_NAME}.trash-{}", std::process::id()))
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
    /// Sessions still running against the data tree, named in the prompt.
    pub live_sessions: Vec<String>,
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

    // Live sessions only matter when their tree is about to go away.
    let live = if remove_data {
        live_sessions(&data)
    } else {
        Vec::new()
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
        live_sessions: live,
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

        // The steps are listed in the order they run: the binary goes last
        // because once it is gone there is no `phimint uninstall` left to
        // retry with.
        let mut step = 1;
        if self.remove_data {
            out.push_str(&format!(
                "  {step}. delete {}          — {} (config.json has your API key)\n",
                self.data_dir.display(),
                format_bytes(self.data_bytes)
            ));
        } else {
            out.push_str(&format!(
                "  {step}. keep {}        — {} (--keep-data)\n",
                self.data_dir.display(),
                format_bytes(self.data_bytes)
            ));
        }
        step += 1;

        if self.remove_data && !self.live_sessions.is_empty() {
            out.push_str(&format!(
                "  ! {} phimint sessions are still running: {}\n\
                 \x20   close them first — their logs write into this tree while it is deleted\n",
                self.live_sessions.len(),
                self.live_sessions.join(", ")
            ));
        }

        match &self.path_entry {
            Some(dir) => out.push_str(&format!("  {step}. drop this from user PATH — {dir}\n")),
            None => out.push_str(&format!("  {step}. (no PATH entry to clean)\n")),
        }
        step += 1;

        for rc in &self.rc_blocks {
            out.push_str(&format!(
                "  {step}. remove the PATH block from {}\n",
                rc.display()
            ));
            step += 1;
        }

        match &self.binary_command {
            Some(cmd) => out.push_str(&format!("  {step}. remove the binary      — `{cmd}`\n")),
            None => out.push_str(&format!(
                "  {step}. remove the binary      — {}\n",
                self.binary_path.display()
            )),
        }

        out
    }
}

/// Ask a yes/no question. `default_yes` is what an empty answer means: the
/// main prompt defaults to yes, the live-session gate to no — an Enter key
/// alone never deletes under running sessions.
///
/// Destructive, so a non-interactive stdin never counts as consent: a pipe or
/// a closed stdin used to read back an empty string, which the `[Y/n]` prompt
/// would have taken as "yes". Without a human at the terminal the only way
/// through is an explicit `--yes`.
fn confirm_with(
    assume_yes: bool,
    prompt: &str,
    default_yes: bool,
    tty: bool,
    input: &mut impl std::io::BufRead,
) -> Result<bool> {
    use std::io::Write as _;

    if assume_yes {
        return Ok(true);
    }
    if !tty {
        println!(
            "Refusing to uninstall without confirmation: stdin is not a terminal.\n\
             Re-run with --yes to proceed."
        );
        return Ok(false);
    }

    print!("{prompt}");
    std::io::stdout().flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;
    let answer = answer.trim().to_lowercase();
    Ok(if answer.is_empty() {
        default_yes
    } else {
        answer == "y" || answer == "yes"
    })
}

/// Carry out a confirmed plan.
pub fn run(plan: &Plan, assume_yes: bool) -> Result<()> {
    use std::io::IsTerminal as _;

    let tty = std::io::stdin().is_terminal();
    let mut input = std::io::stdin().lock();
    run_with(plan, assume_yes, tty, &mut input)
}

/// The testable core of `run`.
pub(crate) fn run_with(
    plan: &Plan,
    assume_yes: bool,
    tty: bool,
    input: &mut impl std::io::BufRead,
) -> Result<()> {
    if !confirm_with(assume_yes, "\nContinue? [Y/n] ", true, tty, input)? {
        println!("Aborted. Nothing was changed.");
        return Ok(());
    }

    // The one extra gate: deleting the data tree under live sessions kills
    // their state, so it takes a second, default-no confirmation of its own.
    if plan.remove_data && !plan.live_sessions.is_empty() {
        println!(
            "\n{} phimint sessions are still running: {}\n\
             \x20   their state and logs are deleted under them",
            plan.live_sessions.len(),
            plan.live_sessions.join(", ")
        );
        if !confirm_with(assume_yes, "Delete anyway? [y/N] ", false, tty, input)? {
            println!("Aborted. Nothing was changed.");
            return Ok(());
        }
    }

    // The binary is removed LAST: once it is gone there is no
    // `phimint uninstall` left to retry with, so every step that can fail
    // runs while the command is still available.

    // 1. Data — the step live sessions race, and the one that failed halfway
    // on 2026-11-21 ("Directory not empty (os error 66)").
    if plan.remove_data && plan.data_dir.exists() {
        delete_data_dir(&plan.data_dir)?;
    } else if plan.data_dir.exists() {
        println!("Kept {}", plan.data_dir.display());
    }

    // 2. Windows user PATH.
    if let Some(dir) = &plan.path_entry {
        strip_user_path(dir)?;
    }

    // 3. The marker block the installer put into the rc file.
    for rc in &plan.rc_blocks {
        if strip_marker_block_file(rc)? {
            println!("Removed the phimint PATH block from {}", rc.display());
        }
    }

    // 4. Binary.
    match &plan.binary_command {
        Some(cmd) => remove_via_manager(cmd)?,
        None => remove_self(&plan.binary_path)?,
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
    if let Err(err) = std::fs::remove_file(exe) {
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            // `install.sh` can fall back to `sudo` for /usr/local/bin; the
            // binary is then root-owned and a normal user cannot unlink it.
            // Name the finishing command, and warn off `sudo phimint
            // uninstall` — sudo's HOME is /root, so the user's own data
            // tree and rc block would be skipped.
            anyhow::bail!(
                "failed to delete {}: {err}\n  \
                 the binary is owned by another user (a sudo install); remove it with:\n  \
                 \x20 sudo rm {}\n  \
                 (do not run `sudo phimint uninstall`: sudo's HOME is /root, so your\n  \
                 \x20 ~/.phimint and the PATH block in your rc file would be skipped)",
                exe.display(),
                exe.display()
            );
        }
        return Err(err).with_context(|| format!("failed to delete {}", exe.display()));
    }
    println!("Removed {}", exe.display());
    Ok(())
}

#[cfg(windows)]
fn remove_self_inner(exe: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: the helper gets no console window.
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let spec = format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{}\"", exe.display());
    // `raw_arg`, not `arg`: std quotes for the C runtime and escapes the
    // path's quotes as `\"`, which cmd does not decode — `del` then sees
    // `\C:\...\` and fails, so the binary survived every Windows uninstall
    // (2026-11-21). The raw form keeps the quotes cmd actually parses.
    Command::new("cmd")
        .arg("/C")
        .raw_arg(&spec)
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
