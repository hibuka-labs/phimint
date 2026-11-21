//! Cross-platform path primitives — the single throat for home lookup.
//!
//! Windows sets USERPROFILE, not HOME. Raw lookups are banned: the CI
//! `platform-gate` job rejects hand-rolled HOME/USERPROFILE env lookups at
//! any call site, so everything that needs home goes through here.

use std::path::PathBuf;

/// The user's home directory with a loud, explicit fallback.
///
/// A missing home must never silently degrade to a relative path (that is how
/// the model-route audit landed in the project cwd on Windows): warn and fall
/// back to the system temp dir so the failure mode is visible and still
/// absolute.
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| {
        tracing::warn!("HOME/USERPROFILE is not set; using temp dir as home fallback");
        std::env::temp_dir()
    })
}
