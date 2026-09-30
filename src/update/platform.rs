//! Platform detection.
//!
//! Maps Rust's `std::env::consts` to the manifest's platform key format.
//! Manifest keys follow Tauri conventions: `darwin`/`linux`/`windows` + `x86_64`/`aarch64`.

use super::error::UpdateError;

/// Return the manifest platform key for the current runtime.
///
/// Mapping: `macos → darwin`, everything else unchanged.
/// Returns e.g. `"darwin-aarch64"`, `"linux-x86_64"`, `"windows-x86_64"`.
pub fn platform_key() -> Result<&'static str, UpdateError> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    match (os, arch) {
        ("macos", "aarch64") => Ok("darwin-aarch64"),
        ("macos", "x86_64") => Ok("darwin-x86_64"),
        ("linux", "x86_64") => Ok("linux-x86_64"),
        ("linux", "aarch64") => Ok("linux-aarch64"),
        ("windows", "x86_64") => Ok("windows-x86_64"),
        _ => Err(UpdateError::UnsupportedPlatform {
            os: os.to_string(),
            arch: arch.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_platform_is_valid() {
        // On any supported dev machine, platform_key() should succeed.
        // We can't test cross-platform mappings without mocking std::env::consts,
        // but we can verify the current platform resolves.
        let key = platform_key();
        assert!(
            key.is_ok(),
            "current platform should be supported: {:?}",
            key.err()
        );

        // Verify the key contains expected substrings
        let key = key.unwrap();
        assert!(
            key.starts_with("darwin-") || key.starts_with("linux-") || key.starts_with("windows-"),
            "unexpected platform key: {}",
            key
        );
    }
}
