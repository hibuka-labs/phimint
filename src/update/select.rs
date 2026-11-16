//! Concurrent manifest race: the first mirror that answers wins.
//!
//! Both mirrors are fetched at the same time. The first response that parses
//! as a valid manifest wins; a mirror that cannot connect is simply absent
//! from the result. The runner-up's manifest is retained so the asset
//! fallback can find the same version on the other mirror without a second
//! network round-trip.

use crate::update::manifest::Manifest;

/// How long to wait for a mirror to answer before giving up on it.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The outcome of the race: the winner, and the runner-up when there was one.
#[derive(Debug, Clone)]
pub struct Picked {
    /// The mirror that won — empty string when no mirror answered.
    pub winner: (String, Manifest),
    /// The other mirror's manifest, when it also answered.
    pub runner_up: Option<(String, Manifest)>,
}

/// Choose the winner and runner-up from the mirrors that answered.
///
/// `entries` is in endpoint order; the first `Ok` is the winner and the
/// second `Ok` is the runner-up. A mirror whose manifest failed to parse is
/// treated as a failure — a malformed manifest must never win the race.
pub fn pick(entries: Vec<(String, Result<Manifest, String>)>) -> Picked {
    let mut winner: Option<(String, Manifest)> = None;
    let mut runner_up: Option<(String, Manifest)> = None;

    for (name, result) in entries {
        let Ok(manifest) = result else { continue };
        if winner.is_none() {
            winner = Some((name, manifest));
        } else if runner_up.is_none() {
            runner_up = Some((name, manifest));
        }
    }

    match winner {
        Some(w) => Picked {
            winner: w,
            runner_up,
        },
        None => Picked {
            winner: (String::new(), Manifest::empty()),
            runner_up,
        },
    }
}

/// Fetch every endpoint concurrently and pick the winner.
///
/// The endpoints are tried at the same time, so an unreachable mirror costs
/// one timeout in parallel rather than one timeout per endpoint in series.
pub async fn race(client: &reqwest::Client, endpoints: &[String]) -> Picked {
    let mut tasks = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        let client = client.clone();
        let endpoint = endpoint.clone();
        tasks.push(tokio::spawn(async move {
            let name = mirror_name(&endpoint);
            let result = fetch_one(&client, &endpoint).await;
            (name, result)
        }));
    }

    let mut entries = Vec::with_capacity(tasks.len());
    for task in tasks {
        match task.await {
            Ok(entry) => entries.push(entry),
            Err(e) => tracing::debug!(error = %e, "manifest race task panicked"),
        }
    }
    pick(entries)
}

/// Fetch and parse one manifest endpoint.
async fn fetch_one(client: &reqwest::Client, endpoint: &str) -> Result<Manifest, String> {
    let resp = client
        .get(endpoint)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    crate::update::manifest::parse(&bytes).map_err(|e| e.to_string())
}

/// The mirror a URL belongs to, for logging and fallback lookup.
pub fn mirror_name(url: &str) -> String {
    if url.contains("gitee.com") {
        "gitee".to_string()
    } else if url.contains("github.com") {
        "github".to_string()
    } else {
        "custom".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::manifest::Manifest;
    use std::collections::HashMap;

    fn manifest(version: &str, url: &str) -> Manifest {
        let mut platforms = HashMap::new();
        platforms.insert(
            "linux-x86_64".to_string(),
            crate::update::manifest::PlatformEntry {
                signature: String::new(),
                url: url.to_string(),
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
    fn winner_is_the_first_mirror_to_answer() {
        let entries = vec![
            (
                "github".to_string(),
                Ok(manifest("0.11.0", "https://github.com/a")),
            ),
            (
                "gitee".to_string(),
                Ok(manifest("0.11.0", "https://gitee.com/a")),
            ),
        ];
        let picked = pick(entries);
        assert_eq!(picked.winner.0, "github");
    }

    #[test]
    fn a_failed_mirror_is_skipped() {
        let entries = vec![
            ("github".to_string(), Err("connect timeout".to_string())),
            (
                "gitee".to_string(),
                Ok(manifest("0.11.0", "https://gitee.com/a")),
            ),
        ];
        let picked = pick(entries);
        assert_eq!(picked.winner.0, "gitee");
    }

    #[test]
    fn the_runner_up_is_kept_for_the_fallback() {
        let entries = vec![
            (
                "github".to_string(),
                Ok(manifest("0.11.0", "https://github.com/a")),
            ),
            (
                "gitee".to_string(),
                Ok(manifest("0.11.0", "https://gitee.com/a")),
            ),
        ];
        let picked = pick(entries);
        let runner_up = picked.runner_up.expect("runner-up kept");
        assert_eq!(runner_up.0, "gitee");
        assert_eq!(
            runner_up.1.platforms["linux-x86_64"].url,
            "https://gitee.com/a"
        );
    }

    #[test]
    fn a_failed_runner_up_is_not_kept() {
        let entries = vec![
            (
                "github".to_string(),
                Ok(manifest("0.11.0", "https://github.com/a")),
            ),
            ("gitee".to_string(), Err("connect timeout".to_string())),
        ];
        let picked = pick(entries);
        assert!(picked.runner_up.is_none());
    }

    #[test]
    fn no_mirror_answers_yields_no_winner() {
        let entries = vec![
            ("github".to_string(), Err("connect timeout".to_string())),
            ("gitee".to_string(), Err("connect timeout".to_string())),
        ];
        let picked = pick(entries);
        assert!(picked.winner.0.is_empty());
    }

    #[test]
    fn the_mirror_name_comes_from_the_url() {
        assert_eq!(mirror_name("https://gitee.com/x/m.json"), "gitee");
        assert_eq!(mirror_name("https://github.com/x/m.json"), "github");
        assert_eq!(mirror_name("https://example.com/m.json"), "custom");
    }
}
