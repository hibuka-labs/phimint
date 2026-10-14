//! Tips cache persistence.
//!
//! Stores the last fetched tips in `~/.phimint/tips-cache.json`, mirroring the
//! update checker's `state.json`: fs2 file locking for concurrent write
//! safety, corrupt files fall back to the built-in defaults.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::manifest::TipItem;
use crate::update::error::UpdateError;

/// The last successful tips fetch. Persisted in `~/.phimint/tips-cache.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TipsCache {
    /// Timestamp of the last successful fetch; drives the refresh interval.
    pub fetched_at: DateTime<Utc>,
    /// Tips manifest version the items came from (date stamp, equality-only).
    pub version: String,
    /// The items to show.
    pub items: Vec<TipItem>,
}

fn cache_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".phimint").join("tips-cache.json"))
}

/// Load the tips cache. Returns `None` when the file is missing, the home
/// directory is unknown, or the file is corrupt (logged, treated as absent).
pub fn load() -> Option<TipsCache> {
    let path = cache_path()?;
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(data) => match serde_json::from_str::<TipsCache>(&data) {
            Ok(cache) => Some(cache),
            Err(e) => {
                tracing::warn!(error = %e, "tips-cache.json corrupt, using built-in tips");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to read tips-cache.json, using built-in tips");
            None
        }
    }
}

/// Save the tips cache to disk with file locking.
pub fn save(cache: &TipsCache) -> Result<(), UpdateError> {
    let path = cache_path().ok_or_else(|| {
        UpdateError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "home directory not found",
        ))
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)?;

    file.lock_exclusive()?;
    let result = serde_json::to_writer_pretty(&file, cache);
    file.unlock()?;

    result.map_err(UpdateError::from)
}

/// Whether the cache is younger than `interval` (a future `fetched_at` counts
/// as fresh; a stale cache is refetched on the next refresh attempt).
pub fn is_fresh(cache: &TipsCache, interval: Duration) -> bool {
    let age = Utc::now().signed_duration_since(cache.fetched_at);
    let limit = TimeDelta::try_seconds(interval.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or(TimeDelta::MAX);
    age < limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tips::TipItem;

    #[test]
    fn roundtrip() {
        let cache = TipsCache {
            fetched_at: DateTime::parse_from_rfc3339("2026-10-13T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            version: "2026-10-13".to_string(),
            items: vec![TipItem::tip("hello")],
        };
        let json = serde_json::to_string_pretty(&cache).unwrap();
        let loaded: TipsCache = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.fetched_at, cache.fetched_at);
        assert_eq!(loaded.version, "2026-10-13");
        assert_eq!(loaded.items, cache.items);
    }

    #[test]
    fn is_fresh_respects_interval() {
        let cache = TipsCache {
            fetched_at: Utc::now() - chrono::Duration::hours(2),
            version: "2026-10-13".to_string(),
            items: vec![TipItem::tip("x")],
        };
        assert!(!is_fresh(&cache, Duration::from_secs(3600)));
        assert!(is_fresh(&cache, Duration::from_secs(86_400)));
    }

    #[test]
    fn future_fetched_at_counts_as_fresh() {
        let cache = TipsCache {
            fetched_at: Utc::now() + chrono::Duration::minutes(5),
            version: "2026-10-13".to_string(),
            items: vec![TipItem::tip("x")],
        };
        assert!(is_fresh(&cache, Duration::from_secs(3600)));
    }
}
