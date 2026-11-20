//! Concurrent manifest race: the first mirror that answers wins.
//!
//! Both mirrors are fetched at the same time. The first response that parses
//! as a valid manifest wins; a mirror that cannot connect is simply absent
//! from the result. The runner-up's manifest is retained so the asset
//! fallback can find the same version on the other mirror without a second
//! network round-trip.
//!
//! "First to answer" means first to *complete*, not first in the endpoint
//! list. The list order is a tiebreaker only: a GitHub endpoint that hangs
//! for 2.9s must not beat a Gitee endpoint that answered in 0.4s, because
//! the whole point of the race is to skip the slow mirror.

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

/// One mirror's answer.
struct Answered {
    name: String,
    manifest: Manifest,
}

/// Choose the winner and runner-up from the mirrors that answered.
///
/// The entries are in *arrival* order: the first mirror to answer wins. A
/// mirror whose manifest failed to parse is treated as a failure — a
/// malformed manifest must never win the race.
pub fn pick(entries: Vec<(String, Result<Manifest, String>)>) -> Picked {
    let mut answered: Vec<Answered> = Vec::new();

    for (name, result) in entries {
        let Ok(manifest) = result else { continue };
        answered.push(Answered { name, manifest });
    }

    let mut winner: Option<(String, Manifest)> = None;
    let mut runner_up: Option<(String, Manifest)> = None;

    for answer in answered {
        if winner.is_none() {
            winner = Some((answer.name, answer.manifest));
        } else if runner_up.is_none() {
            runner_up = Some((answer.name, answer.manifest));
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
///
/// The winner is the mirror that *answered first*, not the one listed first:
/// the futures are polled in completion order, so a mirror that hangs costs
/// the user nothing while the fast one is ready.
pub async fn race(client: &reqwest::Client, endpoints: &[String]) -> Picked {
    let mut tasks = tokio::task::JoinSet::new();
    for endpoint in endpoints {
        let client = client.clone();
        let endpoint = endpoint.clone();
        tasks.spawn(async move {
            let name = mirror_name(&endpoint);
            let result = fetch_one(&client, &endpoint).await;
            (name, result)
        });
    }

    // `join_next` yields the tasks in the order they *complete*, which is the
    // arrival order the race wants: a mirror that hangs costs the user
    // nothing while the fast one is ready.
    let mut entries = Vec::with_capacity(endpoints.len());
    while let Some(result) = tasks.join_next().await {
        match result {
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

    #[test]
    fn the_winner_is_the_first_to_arrive_not_the_first_listed() {
        // The entries arrive in completion order. A mirror listed first but
        // slower must not win: the race exists to skip the slow mirror.
        let entries = vec![
            (
                "gitee".to_string(),
                Ok(manifest("0.11.0", "https://gitee.com/a")),
            ),
            (
                "github".to_string(),
                Ok(manifest("0.11.0", "https://github.com/a")),
            ),
        ];
        let picked = pick(entries);
        assert_eq!(picked.winner.0, "gitee");
        assert_eq!(picked.runner_up.expect("runner-up kept").0, "github");
    }

    #[test]
    fn a_malformed_manifest_never_wins_the_race() {
        // A mirror that answers but serves garbage is a failure, not a winner.
        let entries = vec![
            (
                "github".to_string(),
                Err("manifest parse failed".to_string()),
            ),
            (
                "gitee".to_string(),
                Ok(manifest("0.11.0", "https://gitee.com/a")),
            ),
        ];
        let picked = pick(entries);
        assert_eq!(picked.winner.0, "gitee");
        assert!(picked.runner_up.is_none());
    }

    /// A one-shot HTTP server that answers after `delay_ms` with a manifest.
    ///
    /// The race is a timing behaviour, so the test needs a server whose
    /// answer time it controls — a stub client cannot express "slow mirror".
    ///
    /// The manifest's asset URL carries the server's own port, so the test
    /// can tell the two mirrors apart by the URL that came back.
    async fn spawn_slow_manifest_server(delay_ms: u64, version: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = format!(
            r#"{{"version":"{version}","platforms":{{"linux-x86_64":{{"signature":"s","url":"http://{addr}/asset"}}}}}}"#
        );

        tokio::spawn(async move {
            // Accept once, answer once.
            if let Ok((mut stream, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        format!("http://{addr}/manifest.json")
    }

    /// The port of a manifest endpoint — the identity of the mirror that
    /// served it.
    fn endpoint_port(endpoint: &str) -> u16 {
        endpoint
            .rsplit_once(':')
            .and_then(|(_, rest)| rest.split('/').next())
            .and_then(|p| p.parse().ok())
            .expect("endpoint carries a port")
    }

    #[tokio::test]
    async fn the_slower_mirror_loses_the_race() {
        // The slow mirror is listed FIRST and the fast one second. The winner
        // must be the fast one — otherwise the race pays the slow mirror's
        // latency on every check.
        let slow = spawn_slow_manifest_server(600, "0.11.0").await;
        let fast = spawn_slow_manifest_server(0, "0.11.0").await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .connect_timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();

        let picked = race(&client, &[slow.clone(), fast.clone()]).await;

        let winner_url = picked.winner.1.platforms["linux-x86_64"].url.clone();
        assert!(
            winner_url.contains(&format!(":{}", endpoint_port(&fast))),
            "the fast mirror must win, got {winner_url}"
        );
        let runner_up = picked.runner_up.expect("the slow mirror is the runner-up");
        assert!(
            runner_up.1.platforms["linux-x86_64"]
                .url
                .contains(&format!(":{}", endpoint_port(&slow))),
            "the slow mirror is the runner-up"
        );
    }

    #[tokio::test]
    async fn an_unreachable_mirror_does_not_stop_the_race() {
        // A port nothing listens on: the mirror fails, the other answers.
        let dead = "http://127.0.0.1:1/manifest.json".to_string();
        let live = spawn_slow_manifest_server(0, "0.11.0").await;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .connect_timeout(std::time::Duration::from_secs(1))
            .build()
            .unwrap();

        let picked = race(&client, &[dead, live.clone()]).await;
        let winner_url = picked.winner.1.platforms["linux-x86_64"].url.clone();
        assert!(
            winner_url.contains(&format!(":{}", endpoint_port(&live))),
            "the reachable mirror wins, got {winner_url}"
        );
        assert!(
            picked.runner_up.is_none(),
            "the dead mirror has no manifest"
        );
    }
}
