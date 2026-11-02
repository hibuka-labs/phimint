//! Tips manifest parsing.
//!
//! The cloud `tips.json` format:
//!
//! ```json
//! {
//!   "version": "2026-10-13",
//!   "items": [
//!     { "kind": "note", "text": "phimint is built on phi-agent - docs: ..." },
//!     { "kind": "tip", "text": "Press Ctrl+Y to copy the last reply" }
//!   ]
//! }
//! ```
//!
//! `version` is a date stamp; same-day revisions add a ".2"/".3" suffix and
//! clients compare it for equality. Unknown fields are dropped (serde
//! default), so the manifest may grow without breaking older clients.

use serde::{Deserialize, Serialize};

use crate::update::error::UpdateError;

/// Whether an item is usage advice or a promotional note, and one tips line —
/// the band vocabulary is the shell's ([`phi_agent_tui::TipItem`] /
/// [`phi_agent_tui::TipKind`]); the manifest format and its validation below
/// are product.
pub use phi_agent_tui::{TipItem, TipKind};

/// The cloud tips manifest (see module docs for the JSON format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TipsManifest {
    /// Date-stamped content version, e.g. "2026-10-13" or "2026-10-13.2".
    pub version: String,
    /// The tips to show; order is preserved and rotated through in order.
    #[serde(default)]
    pub items: Vec<TipItem>,
}

/// Parse a tips manifest from JSON bytes.
pub fn parse(data: &[u8]) -> Result<TipsManifest, UpdateError> {
    let manifest: TipsManifest =
        serde_json::from_slice(data).map_err(|e| UpdateError::ManifestParse(e.to_string()))?;

    if manifest.version.is_empty() {
        return Err(UpdateError::ManifestParse(
            "tips manifest has empty version".to_string(),
        ));
    }
    if manifest.items.is_empty() {
        return Err(UpdateError::ManifestParse(
            "tips manifest has no items".to_string(),
        ));
    }
    if manifest.items.iter().any(|i| i.text.is_empty()) {
        return Err(UpdateError::ManifestParse(
            "tips manifest has an item with empty text".to_string(),
        ));
    }

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_happy() {
        let json = r#"{
            "version": "2026-10-13",
            "items": [
                { "kind": "note", "text": "promo" },
                { "kind": "tip", "text": "advice" }
            ]
        }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.version, "2026-10-13");
        assert_eq!(m.items.len(), 2);
        assert_eq!(m.items[0].kind, TipKind::Note);
        assert_eq!(m.items[1].kind, TipKind::Tip);
    }

    #[test]
    fn parse_missing_kind_defaults_to_tip() {
        let json = r#"{ "version": "2026-10-13", "items": [ { "text": "advice" } ] }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.items[0].kind, TipKind::Tip);
    }

    #[test]
    fn parse_same_day_suffix_version() {
        let json = r#"{ "version": "2026-10-13.2", "items": [ { "text": "x" } ] }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.version, "2026-10-13.2");
    }

    #[test]
    fn parse_unknown_fields_ignored() {
        let json = r#"{
            "version": "2026-10-13",
            "refresh_hint": "ignored",
            "items": [ { "text": "x", "priority": 9 } ]
        }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.items[0].text, "x");
    }

    #[test]
    fn parse_empty_items_rejected() {
        let json = r#"{ "version": "2026-10-13", "items": [] }"#;
        assert!(parse(json.as_bytes()).is_err());
    }

    #[test]
    fn parse_empty_version_rejected() {
        let json = r#"{ "version": "", "items": [ { "text": "x" } ] }"#;
        assert!(parse(json.as_bytes()).is_err());
    }

    #[test]
    fn parse_empty_text_rejected() {
        let json = r#"{ "version": "2026-10-13", "items": [ { "text": "" } ] }"#;
        assert!(parse(json.as_bytes()).is_err());
    }

    #[test]
    fn parse_unknown_kind_rejected() {
        let json = r#"{ "version": "2026-10-13", "items": [ { "kind": "ad", "text": "x" } ] }"#;
        assert!(parse(json.as_bytes()).is_err());
    }

    #[test]
    fn parse_malformed_json() {
        assert!(parse(b"not json").is_err());
    }
}
