//! Manifest parsing.
//!
//! Parses the family upgrade manifest (v1) into typed structs.
//! The manifest format is a superset of tauri-plugin-updater's format,
//! with `channel` and `mandatory` extension fields.

use std::collections::HashMap;

use serde::Deserialize;

use super::error::UpdateError;

/// A single platform entry in the manifest.
#[derive(Debug, Clone, Deserialize)]
pub struct PlatformEntry {
    /// minisign signature of the binary.
    pub signature: String,
    /// Download URL for the binary.
    pub url: String,
}

/// The family upgrade manifest (v1).
///
/// JSON format:
/// ```json
/// {
///   "version": "0.2.0",
///   "notes": "...",
///   "pub_date": "2026-09-20T00:00:00Z",
///   "channel": "stable",
///   "mandatory": false,
///   "platforms": {
///     "darwin-aarch64": { "signature": "...", "url": "..." },
///     ...
///   }
/// }
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Semantic version of the release (e.g. "0.2.0", "0.3.0-beta.1").
    pub version: String,
    /// Human-readable release notes (optional).
    #[serde(default)]
    pub notes: Option<String>,
    /// ISO 8601 publication date (optional).
    #[serde(default)]
    pub pub_date: Option<String>,
    /// Release channel: "stable" or "beta". Defaults to "stable".
    #[serde(default = "default_channel")]
    pub channel: String,
    /// Whether this is a mandatory upgrade. Defaults to false.
    /// v1: parsed but not consumed by CLI (Tauri application dialog implements).
    #[serde(default)]
    pub mandatory: bool,
    /// Per-platform binary entries. Key format: `{os}-{arch}` (e.g. "darwin-aarch64").
    pub platforms: HashMap<String, PlatformEntry>,
}

fn default_channel() -> String {
    "stable".to_string()
}

/// Parse a manifest from JSON bytes.
pub fn parse(data: &[u8]) -> Result<Manifest, UpdateError> {
    let manifest: Manifest =
        serde_json::from_slice(data).map_err(|e| UpdateError::ManifestParse(e.to_string()))?;

    // Validate: must have at least one platform
    if manifest.platforms.is_empty() {
        return Err(UpdateError::ManifestParse(
            "manifest has no platforms".to_string(),
        ));
    }

    // Validate: version must be parseable as semver
    semver::Version::parse(&manifest.version).map_err(|e| UpdateError::Semver(e.to_string()))?;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_happy() {
        let json = r#"{
            "version": "0.2.0",
            "notes": "bug fixes",
            "pub_date": "2026-09-20T00:00:00Z",
            "channel": "stable",
            "mandatory": false,
            "platforms": {
                "darwin-aarch64": { "signature": "sig1", "url": "https://example.com/darwin-arm64" },
                "linux-x86_64": { "signature": "sig2", "url": "https://example.com/linux-x64" }
            }
        }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.notes.unwrap(), "bug fixes");
        assert_eq!(m.channel, "stable");
        assert!(!m.mandatory);
        assert_eq!(m.platforms.len(), 2);
        assert!(m.platforms.contains_key("darwin-aarch64"));
    }

    #[test]
    fn parse_minimal() {
        let json = r#"{
            "version": "1.0.0",
            "platforms": { "linux-x86_64": { "signature": "s", "url": "u" } }
        }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.channel, "stable"); // default
        assert!(!m.mandatory); // default
    }

    #[test]
    fn parse_prerelease() {
        let json = r#"{
            "version": "0.3.0-beta.1",
            "channel": "beta",
            "platforms": { "darwin-aarch64": { "signature": "s", "url": "u" } }
        }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.version, "0.3.0-beta.1");
        assert_eq!(m.channel, "beta");
    }

    #[test]
    fn parse_empty_platforms() {
        let json = r#"{ "version": "1.0.0", "platforms": {} }"#;
        assert!(parse(json.as_bytes()).is_err());
    }

    #[test]
    fn parse_invalid_version() {
        let json = r#"{
            "version": "not-semver",
            "platforms": { "linux-x86_64": { "signature": "s", "url": "u" } }
        }"#;
        assert!(parse(json.as_bytes()).is_err());
    }

    #[test]
    fn parse_malformed_json() {
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn parse_unknown_fields_ignored() {
        let json = r#"{
            "version": "1.0.0",
            "unknown_field": "should be ignored",
            "platforms": { "linux-x86_64": { "signature": "s", "url": "u" } }
        }"#;
        let m = parse(json.as_bytes()).unwrap();
        assert_eq!(m.version, "1.0.0");
    }
}
