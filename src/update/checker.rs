//! Update checker: endpoint fallback + semver comparison.
//!
//! Tries endpoints in order (3s timeout each), returns the first valid
//! manifest that indicates an upgrade is available.

use semver::Version;

use super::error::UpdateError;
use super::manifest::Manifest;
use super::platform;
use super::state::UpdateState;

/// Result of an update check.
#[derive(Debug, Clone)]
pub enum CheckResult {
    /// No upgrade available (current >= latest).
    UpToDate,
    /// An upgrade is available.
    UpgradeAvailable {
        version: String,
        download_url: String,
        notes: Option<String>,
        /// Expected SHA-256 of the archive, when the manifest carries one.
        sha256: Option<String>,
        /// The mirror whose manifest won the race.
        mirror: String,
        /// The other mirror's manifest, when it also answered. The asset
        /// fallback reads the same version's URL from it, so a failed
        /// download costs no extra network round-trip. Boxed: the variant
        /// would otherwise dwarf `UpToDate` on every result.
        runner_up: Option<Box<(String, Manifest)>>,
    },
}

/// Check for updates by racing every endpoint at once.
///
/// The endpoints are fetched concurrently, so an unreachable mirror costs one
/// timeout in parallel rather than one timeout per endpoint in series. The
/// first mirror whose manifest parses wins; the runner-up's manifest rides
/// along so the asset fallback needs no extra round-trip.
///
/// Returns `Ok(Some(CheckResult))` if a manifest was successfully fetched and evaluated.
/// Returns `Ok(None)` if all endpoints failed (network/parse errors are silently logged).
pub async fn check(
    client: &reqwest::Client,
    endpoints: &[String],
    state: &UpdateState,
    current_version: &str,
) -> Result<Option<CheckResult>, UpdateError> {
    let platform_key = platform::platform_key()?;
    let current =
        Version::parse(current_version).map_err(|e| UpdateError::Semver(e.to_string()))?;

    let picked = super::select::race(client, endpoints).await;
    if picked.winner.0.is_empty() {
        tracing::debug!("all endpoints exhausted");
        return Ok(None);
    }

    let (mirror, manifest) = picked.winner;
    compare(
        &current,
        &manifest,
        platform_key,
        state,
        &mirror,
        picked.runner_up,
    )
}

/// Compare current version against a manifest.
fn compare(
    current: &Version,
    manifest: &Manifest,
    platform_key: &str,
    state: &UpdateState,
    mirror: &str,
    runner_up: Option<(String, Manifest)>,
) -> Result<Option<CheckResult>, UpdateError> {
    // Check if the platform is supported
    let entry = match manifest.platforms.get(platform_key) {
        Some(e) => e,
        None => {
            tracing::info!(platform = %platform_key, "platform not in manifest");
            return Ok(None);
        }
    };

    let latest =
        Version::parse(&manifest.version).map_err(|e| UpdateError::Semver(e.to_string()))?;

    // Skip if user skipped this version
    if state.skipped_version.as_deref() == Some(&manifest.version) {
        tracing::debug!(version = %manifest.version, "version skipped by user");
        return Ok(None);
    }

    // Prerelease → stable handover: a user on `1.0.0-beta.1` must still be
    // offered `1.0.0` when the stable manifest catches up.
    //
    // The downgrade is only meaningful *within* a major: `0.3.0-beta.1` may
    // fall back to `0.2.0`, because the beta was cut from the `0.3` line. But
    // `1.0.0-beta.1` against a stable `0.12.0` is a different product line —
    // offering it would call a downgrade an "upgrade" and send the user
    // backwards across the boundary their beta was meant to cross.
    //
    // (`1.0.0 > 1.0.0-beta.1` holds on its own, so the same-number handover
    // falls through to the normal comparison below. This branch exists for
    // the same-major case only.)
    if current.pre != semver::Prerelease::EMPTY
        && latest.pre == semver::Prerelease::EMPTY
        && current.major == latest.major
        && latest < *current
    {
        return Ok(Some(CheckResult::UpgradeAvailable {
            version: manifest.version.clone(),
            download_url: entry.url.clone(),
            notes: manifest.notes.clone(),
            sha256: entry.sha256.clone(),
            mirror: mirror.to_string(),
            runner_up: runner_up.clone().map(Box::new),
        }));
    }

    // Normal case: upgrade if latest > current
    if latest > *current {
        return Ok(Some(CheckResult::UpgradeAvailable {
            version: manifest.version.clone(),
            download_url: entry.url.clone(),
            notes: manifest.notes.clone(),
            sha256: entry.sha256.clone(),
            mirror: mirror.to_string(),
            runner_up: runner_up.clone().map(Box::new),
        }));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manifest::PlatformEntry;
    use std::collections::HashMap;

    fn make_manifest(version: &str, platform_url: &str) -> Manifest {
        let mut platforms = HashMap::new();
        platforms.insert(
            "linux-x86_64".to_string(),
            PlatformEntry {
                signature: "sig".to_string(),
                url: platform_url.to_string(),
                sha256: None,
            },
        );
        Manifest {
            version: version.to_string(),
            notes: None,
            pub_date: None,
            channel: "stable".to_string(),
            mandatory: false,
            platforms,
        }
    }

    #[test]
    fn compare_upgrade_available() {
        let current = Version::parse("0.1.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        match result {
            Some(CheckResult::UpgradeAvailable { version, .. }) => assert_eq!(version, "0.2.0"),
            _ => panic!("expected UpgradeAvailable"),
        }
    }

    #[test]
    fn compare_up_to_date() {
        let current = Version::parse("0.2.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn compare_current_newer() {
        let current = Version::parse("0.3.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn compare_skipped_version() {
        let current = Version::parse("0.1.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState {
            skipped_version: Some("0.2.0".to_string()),
            ..Default::default()
        };
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn compare_beta_to_stable_downgrade() {
        let current = Version::parse("0.3.0-beta.1").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        // beta→stable: current prerelease > latest stable, but we allow downgrade
        match result {
            Some(CheckResult::UpgradeAvailable { version, .. }) => assert_eq!(version, "0.2.0"),
            _ => panic!("expected UpgradeAvailable for beta→stable downgrade"),
        }
    }

    /// protects: a prerelease user does not get an "upgrade" that crosses a
    /// major boundary backwards. `1.0.0-beta.1` against a stable manifest still
    /// on `0.12.0` is a *downgrade*, not an upgrade — offering it would send
    /// the user to a release their beta was built to replace.
    #[test]
    fn compare_prerelease_does_not_downgrade_across_a_major() {
        let current = Version::parse("1.0.0-beta.1").unwrap();
        let manifest = make_manifest("0.12.0", "https://example.com/0.12.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        assert!(result.is_none(), "no upgrade may be offered across a major");
    }

    /// protects: the prerelease-to-stable handover does work — a user on
    /// `1.0.0-beta.1` is offered `1.0.0`. This is the case the downgrade
    /// branch was written for, and it must survive the fix above.
    #[test]
    fn compare_prerelease_is_offered_the_same_number_stable() {
        let current = Version::parse("1.0.0-beta.1").unwrap();
        let manifest = make_manifest("1.0.0", "https://example.com/1.0.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state, "github", None).unwrap();
        match result {
            Some(CheckResult::UpgradeAvailable { version, .. }) => assert_eq!(version, "1.0.0"),
            _ => panic!("expected UpgradeAvailable for beta→stable handover"),
        }
    }

    #[test]
    fn compare_unsupported_platform() {
        let current = Version::parse("0.1.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(
            &current,
            &manifest,
            "darwin-aarch64",
            &state,
            "github",
            None,
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn the_upgrade_result_carries_the_mirror_and_the_runner_up() {
        let current = Version::parse("0.1.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://gitee.com/0.2.0");
        let state = UpdateState::default();

        let runner_up_manifest = make_manifest("0.2.0", "https://github.com/0.2.0");
        let runner_up = Some(("github".to_string(), runner_up_manifest));

        let result = compare(
            &current,
            &manifest,
            "linux-x86_64",
            &state,
            "gitee",
            runner_up,
        )
        .unwrap();

        match result {
            Some(CheckResult::UpgradeAvailable {
                mirror,
                runner_up,
                download_url,
                ..
            }) => {
                assert_eq!(mirror, "gitee");
                assert_eq!(download_url, "https://gitee.com/0.2.0");
                // The runner-up rides along so the asset fallback can find the
                // same version on the other mirror without another round-trip.
                let (name, other) = *runner_up.expect("runner-up kept");
                assert_eq!(name, "github");
                assert_eq!(
                    other.platforms["linux-x86_64"].url,
                    "https://github.com/0.2.0"
                );
            }
            _ => panic!("expected UpgradeAvailable"),
        }
    }
}
