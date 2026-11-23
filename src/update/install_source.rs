//! Install-source detection: where did this binary come from?
//!
//! Upgrade routing depends on it — brew/npm/cargo manage their own upgrades
//! (`brew upgrade`, `npm i -g`, `cargo install --force`), while standalone
//! installs (curl installer) self-replace via `phimint update`. The installer
//! records the source in `~/.phimint/state.json`; the executable path is the
//! fallback heuristic (Cellar / node_modules / .cargo).

use std::path::Path;

/// How phimint was installed. Drives the upgrade command shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSource {
    /// Homebrew formula (binary lives in the Cellar).
    Brew,
    /// npm / pnpm / yarn global install (binary under node_modules).
    Npm,
    /// `cargo install` (binary in the Cargo bin dir).
    Cargo,
    /// Windows Package Manager (binary under the WinGet packages dir).
    Winget,
    /// Scoop (binary under the scoop apps dir).
    Scoop,
    /// Standalone: curl installer, or a manually dropped binary.
    Standalone,
}

impl InstallSource {
    /// Stable string persisted in `state.json`.
    pub fn as_str(&self) -> &'static str {
        match self {
            InstallSource::Brew => "brew",
            InstallSource::Npm => "npm",
            InstallSource::Cargo => "cargo",
            InstallSource::Winget => "winget",
            InstallSource::Scoop => "scoop",
            InstallSource::Standalone => "standalone",
        }
    }

    /// Parse the persisted `state.json` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "brew" => Some(InstallSource::Brew),
            "npm" => Some(InstallSource::Npm),
            "cargo" => Some(InstallSource::Cargo),
            "winget" => Some(InstallSource::Winget),
            "scoop" => Some(InstallSource::Scoop),
            "standalone" => Some(InstallSource::Standalone),
            _ => None,
        }
    }

    /// The upgrade command shown to the user for this channel.
    pub fn upgrade_command(&self) -> &'static str {
        match self {
            InstallSource::Brew => "brew upgrade phimint",
            InstallSource::Npm => "npm install -g phimint@latest",
            InstallSource::Cargo => "cargo install phimint --force",
            InstallSource::Winget => "winget upgrade hibuka-labs.phimint",
            InstallSource::Scoop => "scoop update phimint",
            // Self-replace: `phimint update` is handled by the CLI subcommand.
            InstallSource::Standalone => "phimint update",
        }
    }

    /// Whether `phimint update` may replace the binary on disk.
    ///
    /// Managed installs (brew/npm/cargo) are owned by their package manager;
    /// writing over their binary breaks checksums and ownership.
    pub fn supports_self_update(&self) -> bool {
        matches!(self, InstallSource::Standalone)
    }

    /// The uninstall command for this channel.
    ///
    /// Mirrors [`Self::upgrade_command`]: managed installs are removed by their
    /// own package manager (and we must never delete their binary ourselves),
    /// while a standalone install owns its file outright.
    pub fn uninstall_command(&self) -> &'static str {
        match self {
            InstallSource::Brew => "brew uninstall phimint",
            InstallSource::Npm => "npm uninstall -g phimint",
            InstallSource::Cargo => "cargo uninstall phimint",
            InstallSource::Winget => "winget uninstall hibuka-labs.phimint",
            InstallSource::Scoop => "scoop uninstall phimint",
            // Self-removal: `phimint uninstall` deletes its own executable.
            InstallSource::Standalone => "phimint uninstall",
        }
    }

    /// Whether a package manager owns this binary and therefore owns removing it.
    pub fn is_managed(&self) -> bool {
        !matches!(self, InstallSource::Standalone)
    }
}

/// Classify an install source from the executable path.
///
/// Path heuristics (checked in order):
/// - Homebrew: Cellar or homebrew prefix in the path
/// - npm/pnpm/yarn: under a `node_modules` tree
/// - cargo: under the Cargo home bin dir
/// - winget: under the WinGet packages dir
/// - scoop: under the scoop apps dir
/// - anything else: standalone
pub fn detect_from_path(exe: &Path) -> InstallSource {
    // Lossy lowercase: paths are case-sensitive on some systems but the
    // markers below are stable ASCII written by the package managers.
    let s = exe.to_string_lossy().to_lowercase();
    if s.contains("/cellar/")
        || s.contains("/homebrew/")
        || s.contains("\\homebrew\\")
        || s.contains("/linuxbrew/")
    {
        InstallSource::Brew
    } else if s.contains("node_modules") {
        InstallSource::Npm
    } else if s.contains("/.cargo/bin/") || s.contains("\\.cargo\\bin\\") {
        InstallSource::Cargo
    } else if s.contains("\\winget\\packages\\") || s.contains("/winget/packages/") {
        InstallSource::Winget
    } else if s.contains("\\scoop\\apps\\") || s.contains("/scoop/apps/") {
        InstallSource::Scoop
    } else {
        InstallSource::Standalone
    }
}

/// Detect the install source of the running binary.
///
/// Priority: explicit `state.json` record (written by the installer) over the
/// path heuristic. Falls back to the heuristic when no record exists.
pub fn detect() -> InstallSource {
    if let Some(state_src) = crate::update::state::load().install_source
        && let Some(src) = InstallSource::parse(&state_src)
    {
        return src;
    }
    match std::env::current_exe() {
        Ok(exe) => detect_from_path(&exe),
        Err(_) => InstallSource::Standalone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn brew_cellar() {
        assert_eq!(
            detect_from_path(&PathBuf::from(
                "/opt/homebrew/Cellar/phimint/0.2.0/bin/phimint"
            )),
            InstallSource::Brew
        );
    }

    #[test]
    fn brew_intel_prefix() {
        assert_eq!(
            detect_from_path(&PathBuf::from(
                "/usr/local/homebrew/Cellar/phimint/0.2.0/bin/phimint"
            )),
            InstallSource::Brew
        );
    }

    #[test]
    fn npm_node_modules() {
        assert_eq!(
            detect_from_path(&PathBuf::from(
                "/Users/x/.nvm/versions/node/v22.0.0/lib/node_modules/phimint/bin/phimint"
            )),
            InstallSource::Npm
        );
    }

    #[test]
    fn cargo_bin() {
        assert_eq!(
            detect_from_path(&PathBuf::from("/Users/x/.cargo/bin/phimint")),
            InstallSource::Cargo
        );
    }

    #[test]
    fn standalone_local_bin() {
        assert_eq!(
            detect_from_path(&PathBuf::from("/Users/x/.local/bin/phimint")),
            InstallSource::Standalone
        );
    }

    #[test]
    fn standalone_usr_local() {
        assert_eq!(
            detect_from_path(&PathBuf::from("/usr/local/bin/phimint")),
            InstallSource::Standalone
        );
    }

    #[test]
    fn winget_packages_dir() {
        assert_eq!(
            detect_from_path(&PathBuf::from(
                r"C:\Users\x\AppData\Local\Microsoft\WinGet\Packages\hibuka-labs.phimint_Microsoft.Winget.Source_8wekyb3d8bbwe\phimint.exe"
            )),
            InstallSource::Winget
        );
    }

    #[test]
    fn scoop_apps_dir() {
        assert_eq!(
            detect_from_path(&PathBuf::from(
                r"C:\Users\x\scoop\apps\phimint\current\phimint.exe"
            )),
            InstallSource::Scoop
        );
    }

    #[test]
    fn roundtrip_str() {
        for src in [
            InstallSource::Brew,
            InstallSource::Npm,
            InstallSource::Cargo,
            InstallSource::Winget,
            InstallSource::Scoop,
            InstallSource::Standalone,
        ] {
            assert_eq!(InstallSource::parse(src.as_str()), Some(src));
        }
        assert_eq!(InstallSource::parse("bogus"), None);
    }

    #[test]
    fn only_standalone_self_updates() {
        assert!(InstallSource::Standalone.supports_self_update());
        assert!(!InstallSource::Brew.supports_self_update());
        assert!(!InstallSource::Npm.supports_self_update());
        assert!(!InstallSource::Cargo.supports_self_update());
        assert!(!InstallSource::Winget.supports_self_update());
        assert!(!InstallSource::Scoop.supports_self_update());
    }

    #[test]
    fn upgrade_commands_distinct() {
        assert_eq!(
            InstallSource::Brew.upgrade_command(),
            "brew upgrade phimint"
        );
        assert_eq!(
            InstallSource::Standalone.upgrade_command(),
            "phimint update"
        );
    }

    #[test]
    fn uninstall_commands_match_channel_owner() {
        assert_eq!(
            InstallSource::Brew.uninstall_command(),
            "brew uninstall phimint"
        );
        assert_eq!(
            InstallSource::Npm.uninstall_command(),
            "npm uninstall -g phimint"
        );
        assert_eq!(
            InstallSource::Cargo.uninstall_command(),
            "cargo uninstall phimint"
        );
        assert_eq!(
            InstallSource::Winget.uninstall_command(),
            "winget uninstall hibuka-labs.phimint"
        );
        assert_eq!(
            InstallSource::Scoop.uninstall_command(),
            "scoop uninstall phimint"
        );
    }

    #[test]
    fn managed_is_the_inverse_of_standalone() {
        for src in [
            InstallSource::Brew,
            InstallSource::Npm,
            InstallSource::Cargo,
            InstallSource::Winget,
            InstallSource::Scoop,
            InstallSource::Standalone,
        ] {
            assert_eq!(src.is_managed(), !src.supports_self_update());
        }
    }
}
