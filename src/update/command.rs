//! The `phimint update` command: check for a new version, then self-replace
//! when this is a standalone install.
//!
//! Never writes over a package-manager-owned binary (brew checksums break) —
//! those installs get their channel's own upgrade command instead.
//!
//! The command lives here rather than in `src/main.rs` so the binary stays
//! under the project's file-size rule, and so the mirror-resolution logic is
//! testable without going through the CLI.

use anyhow::Result;

use crate::model::config;
use crate::update::checker::{self, CheckResult};
use crate::update::install_source;
use crate::update::mirror;
use crate::update::platform;

/// The manifest endpoints to try, in the order they are raced.
///
/// Precedence: an explicit config list wins, then `PHIMINT_MIRROR`, then the
/// built-in mirror pair. The config's *default* list is the built-in pair, so
/// an untouched config must not suppress the race — only a list the user
/// actually wrote counts as an override.
///
/// `env_mirror` is passed in rather than read from the environment here, so
/// the resolution is a pure function of its inputs and the tests do not have
/// to mutate a process-global (which the parallel test runner would race on).
pub fn resolve_endpoints(
    config_path: Option<&str>,
    env_mirror: Option<String>,
) -> Result<Vec<String>> {
    let configured = match config_path {
        Some(p) => {
            config::ModelConfig::from_file(&std::path::PathBuf::from(p))?
                .update
                .endpoints
        }
        None => match config::ModelConfig::from_default_location() {
            Ok(Some(c)) => c.update.endpoints,
            _ => Vec::new(),
        },
    };
    let configured = if configured == mirror::built_in_endpoints() {
        None
    } else {
        Some(configured)
    };
    Ok(mirror::resolve(env_mirror, configured))
}

/// Run `phimint update`.
///
/// `check_only` reports what is available without touching the binary.
/// `config_path` overrides the config file the endpoint list is read from.
pub async fn run(check_only: bool, config_path: Option<&str>) -> Result<()> {
    let endpoints = resolve_endpoints(config_path, mirror::env_mirror())?;

    let current = env!("CARGO_PKG_VERSION");
    let source = install_source::detect();
    let platform_key = platform::platform_key()?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .connect_timeout(std::time::Duration::from_secs(2))
        .build()?;

    let result =
        checker::check(&client, &endpoints, &crate::update::state::load(), current).await?;

    let Some(CheckResult::UpgradeAvailable {
        version,
        download_url,
        sha256,
        mirror: winning_mirror,
        runner_up,
        ..
    }) = result
    else {
        println!("phimint {current} is up to date.");
        return Ok(());
    };

    println!("phimint {version} available (current: {current}) - via {winning_mirror}.");
    if check_only || !source.supports_self_update() {
        // Managed installs (brew/npm/cargo) are upgraded by their manager.
        println!("Upgrade with: {}", source.upgrade_command());
        return Ok(());
    }
    // The other mirror's entry for the same version, when the race got it.
    // The fallback reads the winner's digest: both mirrors publish the same
    // archive, so the same checksum applies.
    let fallback = runner_up.as_deref().and_then(|(_, other)| {
        crate::update::apply::fallback_entry(other, platform_key)
            .map(|entry| (entry.url, entry.sha256.unwrap_or_default()))
    });
    let fallback = fallback
        .as_ref()
        .map(|(url, sha)| (url.as_str(), sha.as_str()));

    println!("Downloading {download_url} ...");
    let path = crate::update::apply::download_and_replace(
        &client,
        &download_url,
        sha256.as_deref(),
        fallback,
    )
    .await?;
    println!("Updated to {version}: {}", path.display());
    println!("Restart phimint to use the new version.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_endpoints_are_the_built_in_pair() {
        // No config, no env var: the two mirrors are raced.
        let endpoints = resolve_endpoints(None, None).unwrap();
        assert_eq!(endpoints, mirror::built_in_endpoints());
    }

    #[test]
    fn the_env_override_pins_one_mirror() {
        let endpoints = resolve_endpoints(None, Some("gitee".to_string())).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert!(endpoints[0].contains("gitee.com"));
    }
}
