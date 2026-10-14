//! Tips: rotating one-liners under the process status line.
//!
//! Items come from a cloud `tips.json` (versioned by date, e.g. "2026-10-13",
//! with ".2"/".3" suffixes for same-day revisions) and are cached in
//! `~/.phimint/tips-cache.json`. The client compares `version` for
//! equality: an equal version only bumps the cache timestamp, content is
//! untouched. A refresh is attempted at most once per `refresh_interval_secs`
//! (measured from the cache's `fetched_at`), always on a background task so
//! startup is never blocked; any failure is silent and the client keeps
//! showing its current items (cached or built-in).
//!
//! This is a one-way GET with no telemetry: the request carries no user data
//! and nothing is ever uploaded. Promotional items are labelled `Note`, never
//! presented as a `Tip`, and the feature is off with `ui.tips.enabled: false`.

pub mod cache;
pub mod manifest;

pub use cache::TipsCache;
pub use manifest::{TipItem, TipKind, TipsManifest};

use std::time::Duration;

use chrono::Utc;

use crate::update::error::UpdateError;

/// Built-in items shown before the first fetch and whenever a fetch fails.
///
/// The first entry is the phi-agent promo, marked `Note` so it never reads as
/// usage advice. The key-hint facts that used to sit on the bottom status bar
/// (send, scroll, quit) rotate here instead. Keep every text ASCII-only: the
/// TUI rejects wide glyphs in chrome
/// (`render_tests::chrome_sources_stay_cjk_width_safe`).
pub fn default_items() -> Vec<TipItem> {
    vec![
        TipItem::note("phimint is built on phi-agent - docs: https://docs.phiagent.dev/"),
        TipItem::tip("Type @ to reference a file in your message, / for commands"),
        TipItem::tip("Press Ctrl+O to fold or unfold the thinking panel"),
        TipItem::tip("Enter sends your message, Shift+Enter starts a new line without sending"),
        TipItem::tip("Press Ctrl+C once to cancel the running task"),
        TipItem::tip("Press Ctrl+C twice to quit when idle"),
        TipItem::tip("Press Ctrl+Y to copy the last reply"),
        TipItem::tip("Scroll history with the mouse wheel or fn+Up/Down"),
    ]
}

/// Items shown before the first successful fetch: cached cloud tips when a
/// usable cache exists, built-in defaults otherwise.
pub fn initial_items() -> Vec<TipItem> {
    match cache::load() {
        Some(c) if !c.items.is_empty() => c.items,
        _ => default_items(),
    }
}

/// Refresh tips when the cache is stale.
///
/// Returns the items to push to the UI, `None` when the cache is still fresh,
/// the fetch failed, or the manifest version equals the cached one (only
/// `fetched_at` is bumped then). Never errors: failures are logged and the
/// caller keeps showing the current items.
pub async fn refresh(
    client: &reqwest::Client,
    endpoints: &[String],
    interval: Duration,
) -> Option<Vec<TipItem>> {
    let cached = cache::load();
    if cached
        .as_ref()
        .is_some_and(|c| cache::is_fresh(c, interval))
    {
        return None;
    }
    let manifest = fetch_manifest(client, endpoints).await?;
    let (fresh, pushed) = merge(cached, manifest, Utc::now());
    if let Err(e) = cache::save(&fresh) {
        tracing::warn!(error = %e, "failed to save tips cache");
    }
    pushed
}

/// Fold a fetched manifest into the cached state.
///
/// Returns the cache to persist and, only when the manifest version differs
/// from the cached one, the items to push to the UI. An equal version is
/// content-stable by contract, so just bump `fetched_at`.
fn merge(
    cached: Option<TipsCache>,
    manifest: TipsManifest,
    now: chrono::DateTime<Utc>,
) -> (TipsCache, Option<Vec<TipItem>>) {
    match cached {
        Some(mut c) if c.version == manifest.version => {
            c.fetched_at = now;
            (c, None)
        }
        _ => {
            let fresh = TipsCache {
                fetched_at: now,
                version: manifest.version,
                items: manifest.items,
            };
            let pushed = fresh.items.clone();
            (fresh, Some(pushed))
        }
    }
}

/// Try endpoints in order (3s timeout each), return the first valid manifest.
async fn fetch_manifest(client: &reqwest::Client, endpoints: &[String]) -> Option<TipsManifest> {
    for endpoint in endpoints {
        match try_endpoint(client, endpoint).await {
            Ok(m) => return Some(m),
            Err(e) => {
                tracing::debug!(endpoint = %endpoint, error = %e, "tips fetch failed, trying next");
                continue;
            }
        }
    }
    tracing::debug!("all tips endpoints exhausted");
    None
}

/// Fetch and parse one tips endpoint.
async fn try_endpoint(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<TipsManifest, UpdateError> {
    let resp = client
        .get(endpoint)
        .timeout(Duration::from_secs(3))
        .send()
        .await?;
    let bytes = resp.bytes().await?;
    manifest::parse(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(version: &str, text: &str) -> TipsManifest {
        TipsManifest {
            version: version.to_string(),
            items: vec![TipItem::tip(text)],
        }
    }

    #[test]
    fn defaults_start_with_the_labelled_promo() {
        let items = default_items();
        assert_eq!(items[0].kind, TipKind::Note);
        assert!(items[0].text.contains("docs.phiagent.dev"));
        assert!(items[1..].iter().all(|i| i.kind == TipKind::Tip));
    }

    #[test]
    fn merge_first_fetch_pushes_items() {
        let (cache, pushed) = merge(None, manifest("2026-10-13", "hello"), Utc::now());
        assert_eq!(cache.version, "2026-10-13");
        assert_eq!(pushed.unwrap()[0].text, "hello");
    }

    #[test]
    fn merge_equal_version_only_bumps_fetched_at() {
        let old = TipsCache {
            fetched_at: Utc::now() - chrono::Duration::days(2),
            version: "2026-10-13".to_string(),
            items: vec![TipItem::tip("cached")],
        };
        let before = old.fetched_at;
        let (cache, pushed) = merge(Some(old), manifest("2026-10-13", "server text"), Utc::now());
        assert!(pushed.is_none());
        assert_eq!(cache.items[0].text, "cached");
        assert!(cache.fetched_at > before);
    }

    #[test]
    fn merge_new_version_replaces_items() {
        let old = TipsCache {
            fetched_at: Utc::now(),
            version: "2026-10-13".to_string(),
            items: vec![TipItem::tip("old")],
        };
        let (cache, pushed) = merge(Some(old), manifest("2026-10-13.2", "new"), Utc::now());
        assert_eq!(cache.version, "2026-10-13.2");
        assert_eq!(pushed.unwrap()[0].text, "new");
    }
}
