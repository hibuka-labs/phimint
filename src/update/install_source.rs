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
            InstallSource::Standalone => "standalone",
        }
    }

    /// Parse the persisted `state.json` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "brew" => Some(InstallSource::Brew),
            "npm" => Some(InstallSource::Npm),
            "cargo" => Some(InstallSource::Cargo),
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
}

/// Classify an install source from the executable path.
///
/// Path heuristics (checked in order):
/// - Homebrew: Cellar or homebrew prefix in the path
/// - npm/pnpm/yarn: under a `node_modules` tree
/// - cargo: under the Cargo home bin dir
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
    fn roundtrip_str() {
        for src in [
            InstallSource::Brew,
            InstallSource::Npm,
            InstallSource::Cargo,
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
}
