//! Update checker: endpoint fallback + semver comparison.
//!
//! Tries endpoints in order (3s timeout each), returns the first valid
//! manifest that indicates an upgrade is available.

use semver::Version;

use super::error::UpdateError;
use super::manifest::{self, Manifest};
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
    },
}

/// Check for updates by trying endpoints in order.
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

    for endpoint in endpoints {
        match try_endpoint(client, endpoint, platform_key, &current, state).await {
            Ok(Some(result)) => return Ok(Some(result)),
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(endpoint = %endpoint, error = %e, "endpoint failed, trying next");
                continue;
            }
        }
    }

    tracing::debug!("all endpoints exhausted");
    Ok(None)
}

/// Try a single endpoint.
async fn try_endpoint(
    client: &reqwest::Client,
    endpoint: &str,
    platform_key: &str,
    current: &Version,
    state: &UpdateState,
) -> Result<Option<CheckResult>, UpdateError> {
    let resp = client
        .get(endpoint)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await?;

    let bytes = resp.bytes().await?;
    let m = manifest::parse(&bytes)?;

    compare(current, &m, platform_key, state)
}

/// Compare current version against a manifest.
fn compare(
    current: &Version,
    manifest: &Manifest,
    platform_key: &str,
    state: &UpdateState,
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

    // Beta → stable downgrade: allow if current is prerelease and latest is not
    if current.pre != semver::Prerelease::EMPTY
        && latest.pre == semver::Prerelease::EMPTY
        && latest < *current
    {
        return Ok(Some(CheckResult::UpgradeAvailable {
            version: manifest.version.clone(),
            download_url: entry.url.clone(),
            notes: manifest.notes.clone(),
            sha256: entry.sha256.clone(),
        }));
    }

    // Normal case: upgrade if latest > current
    if latest > *current {
        return Ok(Some(CheckResult::UpgradeAvailable {
            version: manifest.version.clone(),
            download_url: entry.url.clone(),
            notes: manifest.notes.clone(),
            sha256: entry.sha256.clone(),
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
        let result = compare(&current, &manifest, "linux-x86_64", &state).unwrap();
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
        let result = compare(&current, &manifest, "linux-x86_64", &state).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn compare_current_newer() {
        let current = Version::parse("0.3.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state).unwrap();
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
        let result = compare(&current, &manifest, "linux-x86_64", &state).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn compare_beta_to_stable_downgrade() {
        let current = Version::parse("0.3.0-beta.1").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "linux-x86_64", &state).unwrap();
        // beta→stable: current prerelease > latest stable, but we allow downgrade
        match result {
            Some(CheckResult::UpgradeAvailable { version, .. }) => assert_eq!(version, "0.2.0"),
            _ => panic!("expected UpgradeAvailable for beta→stable downgrade"),
        }
    }

    #[test]
    fn compare_unsupported_platform() {
        let current = Version::parse("0.1.0").unwrap();
        let manifest = make_manifest("0.2.0", "https://example.com/0.2.0");
        let state = UpdateState::default();
        let result = compare(&current, &manifest, "darwin-aarch64", &state).unwrap();
        assert!(result.is_none());
    }
}
