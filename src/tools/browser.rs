//! Browser launch (1C): the one place a canvas URL may reach a browser.
//!
//! Product-shell work — phi-canvas only sees a `BrowserLauncher` decision.
//! The opener argv is a pure function of (platform, url) so all four platform
//! forms are unit-tested on every host (row 20's automation half); the
//! real-machine smoke (macOS in the walkthrough, Windows/WSL deferred) stays
//! on the human checklist.
//!
//! Headless fallback: when no opener can be spawned, the caller shows the URL
//! in the TUI instead — the canvas itself (server, clicks, status bar) works
//! unchanged.

use std::process::{Command, Stdio};

/// Which opener family to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS: `open -g` — `-g` keeps the browser from stealing focus (the
    /// terminal is where the user is working).
    MacOs,
    /// Linux desktop: `xdg-open`.
    Linux,
    /// Windows: `cmd /c start "" <url>` — the `""` is the empty window title
    /// `start` requires before the target.
    Windows,
    /// WSL: `wslview` (wslu) first, falling back to the Windows opener.
    Wsl,
}

/// One spawn attempt: program + argv. Plain data so the construction is
/// unit-testable without a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

/// What happened when we tried to show the URL to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchOutcome {
    /// An opener was spawned.
    Spawned,
    /// No opener worked (headless, or the URL is not a canvas URL) — show the
    /// URL in the TUI and carry on.
    ShowUrl,
}

/// Platform detection from injected facts (pure): `os` is
/// `std::env::consts::OS`, `wsl` is whether we run under WSL.
pub fn detect_platform(os: &str, wsl: bool) -> Platform {
    match os {
        "macos" => Platform::MacOs,
        "windows" => Platform::Windows,
        _ if wsl => Platform::Wsl,
        _ => Platform::Linux,
    }
}

/// [`detect_platform`] over the real environment. WSL shows up either through
/// `WSL_DISTRO_NAME` or a Microsoft kernel string in `/proc/version`.
pub fn current_platform() -> Platform {
    let wsl = std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .map(|version| version.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false);
    detect_platform(std::env::consts::OS, wsl)
}

/// The opener argv for `url` on `platform`, best candidate first (WSL tries
/// `wslview` before the Windows opener).
pub fn launch_commands(platform: Platform, url: &str) -> Vec<CommandSpec> {
    let spec = |program: &str, args: &[&str]| CommandSpec {
        program: program.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
    };
    match platform {
        Platform::MacOs => vec![spec("open", &["-g", url])],
        Platform::Linux => vec![spec("xdg-open", &[url])],
        Platform::Windows => vec![spec("cmd", &["/c", "start", "", url])],
        Platform::Wsl => vec![
            spec("wslview", &[url]),
            spec("cmd.exe", &["/c", "start", "", url]),
        ],
    }
}

/// Only loopback HTTP is ever launched (M4 posture: the canvas lives at
/// `http://127.0.0.1:{port}`; nothing else may ride this port into a browser).
pub fn is_canvas_url(url: &str) -> bool {
    ["http://127.0.0.1:", "http://localhost:"]
        .iter()
        .any(|prefix| url.starts_with(prefix))
}

/// The single spawn site, over the real environment. Tries each platform
/// candidate in order; opener stdout/stderr are discarded so a noisy opener
/// never pollutes the TUI's terminal, and the child is reaped on a detached
/// thread (`xdg-open` may outlive the call; a zombie would linger otherwise).
pub fn launch_browser(url: &str) -> LaunchOutcome {
    launch_with(current_platform(), url, try_spawn)
}

/// Launch attempt with the spawn function injected: candidate order and the
/// headless fallback are then fully testable (no real browser required).
pub fn launch_with(
    platform: Platform,
    url: &str,
    mut spawn: impl FnMut(&CommandSpec) -> bool,
) -> LaunchOutcome {
    if !is_canvas_url(url) {
        return LaunchOutcome::ShowUrl;
    }
    for candidate in launch_commands(platform, url) {
        if spawn(&candidate) {
            return LaunchOutcome::Spawned;
        }
    }
    LaunchOutcome::ShowUrl
}

/// Spawn one opener without touching the terminal. `true` once the process
/// was created (we do not wait for it to succeed — a GUI opener's exit status
/// says nothing about whether a window appeared).
fn try_spawn(spec: &CommandSpec) -> bool {
    let child = Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match child {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(err) => {
            tracing::debug!(program = %spec.program, %err, "browser opener unavailable");
            false
        }
    }
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod browser_tests;
