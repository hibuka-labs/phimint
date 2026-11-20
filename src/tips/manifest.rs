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

    /// The repo's tips source file must stay loadable by the client.
    ///
    /// 2026-11-19: the cloud content source had never worked — no release
    /// ever published a tips.json, so every fetch 404'd and the client
    /// silently kept its built-in tips. The release pipeline now generates
    /// the asset from `tips.json` at the repo root; this test pins the
    /// source against the rules the client enforces, so a bad edit fails the
    /// build instead of failing silently in every user's terminal.
    #[test]
    fn repo_tips_source_satisfies_the_client_contract() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tips.json");
        let raw = std::fs::read(path).expect("repo tips.json is missing");

        // The source carries the items only; the generator stamps `version`.
        let source: serde_json::Value =
            serde_json::from_slice(&raw).expect("tips.json is not JSON");
        assert!(
            source.get("version").is_none(),
            "the source file must not carry a version — the generator stamps it"
        );

        let mut stamped = source.clone();
        stamped["version"] = serde_json::json!("2026-01-01");
        let manifest = parse(stamped.to_string().as_bytes())
            .expect("the repo tips.json does not satisfy the client contract");

        assert!(!manifest.items.is_empty(), "the tips source has no items");
        for item in &manifest.items {
            assert!(!item.text.trim().is_empty(), "a tips item is blank");
            assert!(
                item.text.is_ascii(),
                "a tips item is not ASCII (the TUI rejects wide glyphs in chrome): {}",
                item.text
            );
        }
    }

    /// The tips content must not point at a release mirror.
    ///
    /// One file serves both the GitHub and the Gitee mirror, so a tips item
    /// must not embed a GitHub/Gitee URL: the domestic client would follow
    /// it out of the country. A link to another site (the phi-agent docs,
    /// say) is content, not a mirror dependency — that is allowed.
    #[test]
    fn repo_tips_source_carries_no_mirror_url() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tips.json");
        let raw = std::fs::read(path).expect("repo tips.json is missing");
        let source: serde_json::Value =
            serde_json::from_slice(&raw).expect("tips.json is not JSON");
        let items = source["items"]
            .as_array()
            .expect("tips.json has no items array");
        for item in items {
            let text = item["text"].as_str().unwrap_or_default();
            assert!(
                !text.contains("github.com") && !text.contains("gitee.com"),
                "a tips item points at a release mirror (the domestic client would follow it out of the country): {}",
                text
            );
        }
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

#[cfg(test)]
mod release_roundtrip_tests {
    //! End-to-end: the file the release pipeline publishes must be loadable
    //! by the client that fetches it.
    //!
    //! The generator stamps the version; the client rejects a manifest with
    //! an empty version, an empty item list or an empty item text. If the
    //! generator and the client ever disagree, the symptom is a silent 404
    //! in every user's terminal — so the two sides are checked together here.

    use super::*;

    /// The generator's output shape, as `scripts/gen-manifest.py` writes it.
    fn generated_manifest(items: &str) -> String {
        format!(
            "{{\n  \"version\": \"2026-11-19\",\n  \"items\": {}\n}}",
            items
        )
    }

    #[test]
    fn a_generated_manifest_parses() {
        let raw = generated_manifest(
            r#"[{"kind": "note", "text": "promo"}, {"kind": "tip", "text": "advice"}]"#,
        );
        let m = parse(raw.as_bytes()).expect("the generated manifest must load");
        assert_eq!(m.version, "2026-11-19");
        assert_eq!(m.items.len(), 2);
        assert_eq!(m.items[0].kind, TipKind::Note);
        assert_eq!(m.items[1].kind, TipKind::Tip);
    }

    #[test]
    fn the_shipped_source_becomes_a_loadable_manifest() {
        // The repo's tips.json is the generator's input. Stamping a version
        // on it must produce something the client accepts.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tips.json");
        let raw = std::fs::read(path).expect("repo tips.json is missing");
        let source: serde_json::Value =
            serde_json::from_slice(&raw).expect("tips.json is not JSON");

        let mut stamped = source;
        stamped["version"] = serde_json::json!("2026-11-19");
        let m = parse(stamped.to_string().as_bytes())
            .expect("the shipped tips source must survive the generator's stamp");
        assert!(!m.items.is_empty());
    }
}
