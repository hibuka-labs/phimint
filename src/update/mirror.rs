//! Mirror catalogue: the built-in release endpoints and the override rules.
//!
//! The updater serves the same artifacts from two hosts: GitHub (global) and
//! Gitee (mainland China). Which one is reachable is a property of the
//! caller's network, not of the caller's country, so the updater measures
//! both instead of asking a geo-IP service.
//!
//! Each mirror's `manifest.json` carries asset URLs pointing at its own
//! host: the release pipeline writes `manifest.json` (GitHub URLs) and
//! `manifest-gitee.json` (Gitee URLs) from one checksum map
//! (`scripts/gen-manifest.py`), and uploads the Gitee variant renamed to
//! `manifest.json` on the Gitee release (`deploy/release.sh`). Picking the
//! right manifest therefore picks the right asset host for free.

use std::env;

/// The GitHub manifest endpoint (global).
pub const GITHUB_MANIFEST: &str =
    "https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json";

/// The Gitee manifest endpoint (mainland China).
///
/// The `releases/download/latest/` form is used rather than the
/// `raw/release-metadata/` form: the latter answers 302 through
/// `raw.giteeusercontent.com` and is not a release asset. The file at this
/// path is the Gitee-URL manifest, uploaded renamed by `deploy/release.sh`.
pub const GITEE_MANIFEST: &str =
    "https://gitee.com/chenkangzeng_admin/phimint/releases/download/latest/manifest.json";

/// The two built-in manifest endpoints, GitHub first.
pub fn built_in_endpoints() -> Vec<String> {
    vec![GITHUB_MANIFEST.to_string(), GITEE_MANIFEST.to_string()]
}

/// The manifest endpoint for one named mirror.
pub fn endpoint_for(mirror: &str) -> Option<String> {
    match mirror.trim().to_ascii_lowercase().as_str() {
        "github" => Some(GITHUB_MANIFEST.to_string()),
        "gitee" => Some(GITEE_MANIFEST.to_string()),
        _ => None,
    }
}

/// Resolve the manifest endpoints to try.
///
/// Precedence: an explicit config list wins, then the `PHIMINT_MIRROR`
/// environment variable, then the built-in pair. The environment variable is
/// the same one `install.sh` already honours, so the installer and the
/// updater agree on which mirror the user asked for.
pub fn resolve(env_mirror: Option<String>, configured: Option<Vec<String>>) -> Vec<String> {
    if let Some(list) = configured
        && !list.is_empty()
    {
        return list;
    }
    if let Some(name) = env_mirror
        && let Some(url) = endpoint_for(&name)
    {
        return vec![url];
    }
    built_in_endpoints()
}

/// The `PHIMINT_MIRROR` value from the environment, if set.
pub fn env_mirror() -> Option<String> {
    env::var("PHIMINT_MIRROR")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_endpoints_are_github_and_gitee() {
        let eps = built_in_endpoints();
        assert_eq!(eps.len(), 2);
        assert!(eps[0].contains("github.com/hibuka-labs/phimint"));
        assert!(eps[1].contains("gitee.com/chenkangzeng_admin/phimint"));
    }

    #[test]
    fn gitee_endpoint_is_the_release_asset_form() {
        // The raw/ form 302-redirects through raw.giteeusercontent.com and
        // is not a release asset; the releases/download/ form is a direct
        // 200 carrying the same file.
        let eps = built_in_endpoints();
        assert!(eps[1].contains("/releases/download/latest/manifest.json"));
        assert!(!eps[1].contains("/raw/"));
    }

    #[test]
    fn mirror_override_selects_one_endpoint() {
        let eps = resolve(Some("gitee".to_string()), None);
        assert_eq!(eps.len(), 1);
        assert!(eps[0].contains("gitee.com"));
    }

    #[test]
    fn github_override_selects_one_endpoint() {
        let eps = resolve(Some("github".to_string()), None);
        assert_eq!(eps.len(), 1);
        assert!(eps[0].contains("github.com"));
    }

    #[test]
    fn unknown_override_falls_back_to_both() {
        let eps = resolve(Some("nowhere".to_string()), None);
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn explicit_config_endpoints_win_over_the_env_override() {
        let configured = vec!["https://example.com/m.json".to_string()];
        let eps = resolve(Some("gitee".to_string()), Some(configured.clone()));
        assert_eq!(eps, configured);
    }

    #[test]
    fn an_empty_config_list_does_not_suppress_the_built_in_pair() {
        let eps = resolve(None, Some(Vec::new()));
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn the_env_override_is_read_when_present_and_absent_when_unset() {
        // One test for the one process-global: the test harness runs tests
        // in parallel threads, so two tests mutating PHIMINT_MIRROR would
        // race each other.
        // SAFETY: no other test in this binary touches PHIMINT_MIRROR.
        unsafe { std::env::remove_var("PHIMINT_MIRROR") };
        assert!(env_mirror().is_none());

        unsafe { std::env::set_var("PHIMINT_MIRROR", "gitee") };
        let value = env_mirror();
        assert_eq!(value.as_deref(), Some("gitee"));

        // A blank value counts as unset.
        unsafe { std::env::set_var("PHIMINT_MIRROR", "  ") };
        let blank = env_mirror();
        assert!(blank.is_none());

        unsafe { std::env::remove_var("PHIMINT_MIRROR") };
        assert!(env_mirror().is_none());
    }
}
