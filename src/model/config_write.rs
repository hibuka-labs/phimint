//! File-level top-level member writes for `phimint config set`.
//!
//! Same surgery family as [`crate::model::persist`] (the `/model` pointer):
//! a member-level edit of the raw JSON5 text so the user's comments, key
//! order and trailing commas survive byte for byte. This module adds the
//! generic, any-key form and the file API behind `phimint config set` —
//! including the permission rule (decision 6): the file holds a live key,
//! so every write lands 0600 regardless of what the file was before.

use std::path::Path;

use crate::model::config::ModelConfig;
use crate::model::persist::{self, PersistError, set_member_text};

/// Set the top-level member `key` to the JSON text `value_json` in `path`:
/// refuse an unparseable file first (the user's bytes are never "fixed"
/// silently), run the member surgery, then save through the guarded atomic
/// write. Comments, key order and trailing commas survive byte for byte.
pub fn set_member(path: &Path, key: &str, value_json: &str) -> Result<(), PersistError> {
    let raw = std::fs::read_to_string(path).map_err(PersistError::Read)?;
    json5::from_str::<ModelConfig>(&raw).map_err(|e| PersistError::Parse(e.to_string()))?;
    let updated = set_member_text(&raw, key, value_json)?;
    persist::write_guarded(path, &updated)?;
    tighten(path);
    Ok(())
}

/// [`set_member`] for a plain string value: the value is JSON-escaped first,
/// so a key with quotes or backslashes lands as valid JSON.
pub fn set_member_str(path: &Path, key: &str, value: &str) -> Result<(), PersistError> {
    set_member(path, key, &persist::jstr(value))
}

/// Decision 6: this file holds a live API key — every write lands 0600,
/// whatever umask or old permissions it had (the atomic write preserves the
/// prior mode, so tighten it explicitly afterwards).
fn tighten(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// protects: a write replaces only the value span of the named member —
    /// comments, sibling keys and a look-alike nested in another section
    /// survive byte-for-byte. fails_when: any byte outside the value span
    /// changes, or a nested `api_key` is touched. no_claim: duplicate
    /// top-level keys (first one wins, like the pointer surgery).
    #[test]
    fn a_member_value_is_replaced_and_everything_else_survives() {
        let raw = "{\n  // endpoint for everything\n  \"base_url\": \"https://api.example.com\",\n  \"main\": \"boot-model\",\n  \"api_key\": \"sk-old\", // the key\n  \"ui\": {\"api_key\": \"nested-untouched\"}\n}\n";
        let updated = set_member_text(raw, "api_key", "\"sk-new\"").unwrap();
        assert_eq!(
            updated,
            "{\n  // endpoint for everything\n  \"base_url\": \"https://api.example.com\",\n  \"main\": \"boot-model\",\n  \"api_key\": \"sk-new\", // the key\n  \"ui\": {\"api_key\": \"nested-untouched\"}\n}\n",
            "only the value span moves: {updated}"
        );
    }

    /// protects: an absent member is inserted after the top-level `main`
    /// member with unique comma ownership on both branches, copying the
    /// file's own spacing. fails_when: a missing or stray comma corrupts the
    /// file, or a single-line file grows whitespace. no_claim: files without
    /// a `main` anchor (loud error instead).
    #[test]
    fn an_absent_member_is_inserted_after_the_main_member() {
        // `main` has a right neighbor: comma trails the new member.
        let raw = "{\n  \"main\": \"m\",\n  \"lite\": \"l\"\n}\n";
        assert_eq!(
            set_member_text(raw, "api_key", "\"sk\"").unwrap(),
            "{\n  \"main\": \"m\",\n  \"api_key\": \"sk\",\n  \"lite\": \"l\"\n}\n"
        );

        // `main` is the last member: comma goes before the new one.
        let raw = "{\n  \"main\": \"alpha\",\n  \"api_key\": \"sk\"\n}\n";
        assert_eq!(
            set_member_text(raw, "protocol", "\"openai\"").unwrap(),
            "{\n  \"main\": \"alpha\",\n  \"protocol\": \"openai\",\n  \"api_key\": \"sk\"\n}\n",
            "anchor is the main MEMBER: the new member lands right after it"
        );

        // No anchor at all: loud error, never a guess.
        assert!(set_member_text("{\"api_key\": \"sk\"}", "base_url", "\"u\"").is_err());
    }

    /// protects: the string form escapes the value before splicing — a key
    /// with quotes or backslashes lands as valid JSON. fails_when: a
    /// pathological key breaks the file syntax. no_claim: non-string values
    /// (config members here are all strings).
    #[test]
    fn a_string_value_is_escaped_before_it_lands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{\n  \"main\": \"m\",\n  \"api_key\": \"\"\n}\n").unwrap();
        set_member_str(&path, "api_key", "sk-a\"b\\c").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: crate::model::config::ModelConfig = json5::from_str(&raw).unwrap();
        assert_eq!(
            parsed.api_key.as_deref(),
            Some("sk-a\"b\\c"),
            "round trips: {raw}"
        );
    }

    /// protects: a file that does not parse is refused before any surgery —
    /// the bytes on disk are the user's until a valid edit replaces them.
    /// fails_when: an unparseable file is silently "fixed" or left half
    /// written. no_claim: parser error quality.
    #[test]
    fn an_unparseable_file_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, "{ not json ").unwrap();
        let err = set_member(&path, "api_key", "\"sk\"").unwrap_err();
        assert!(
            matches!(err, crate::model::persist::PersistError::Parse(_)),
            "a parse failure, not a write failure: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not json ",
            "the file survives untouched"
        );
    }

    /// protects: the file round trip keeps comments intact and tightens the
    /// mode to 0600 on every write — the file holds a live key, whatever
    /// umask or old permissions it landed with. fails_when: a 0644 file
    /// keeps its mode after a key write (world-readable key). no_claim:
    /// ACLs or platform permission models beyond unix modes.
    #[test]
    fn the_file_round_trip_preserves_comments_and_tightens_to_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            "{\n  // keep me\n  \"main\": \"m\",\n  \"api_key\": \"\"\n}\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        set_member(&path, "api_key", "\"sk-live\"").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("// keep me"), "comment survives: {raw}");
        assert!(
            raw.contains("\"api_key\": \"sk-live\""),
            "value landed: {raw}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "a key file is never world-readable");
        }
    }
}
