//! Persisting a `/model` pick to the config file.
//!
//! Feedback 2026-11-07: a pick used to be session state only — the next
//! boot fell back to the file's `main`, so the config and the live model
//! disagreed and the model's self-introduction drifted. Requirement: every
//! switch persists, and the file stays as the human wrote it — comments,
//! key order, and bytes outside the written member survive untouched (the
//! file holds a live API key; a whole-file re-serialize would rewrite it).
//!
//! The write is a key-level surgery on the top-level `model` key (the
//! selection pointer — the tier DEFINITIONS `main`/`lite`/`advanced` are
//! never written):
//!
//! 1. **replace value** (key present): splice just the value span;
//! 2. **insert member** (key absent): add `"model": <value>` after the
//!    top-level `main` member, comma ownership on both branches;
//! 3. **delete member** (「clear selection」): drop the member and one
//!    adjacent comma.
//!
//! Comments are never parsed and never rewritten (they may survive as
//! orphans — acceptable). Every output is re-parsed before the write: a
//! splice that does not parse back is refused and the file is left untouched.
//! The write is tmp+rename atomic, and the tmp is created 0600 — it briefly
//! holds the live API key.

use std::fmt;
use std::path::Path;

use crate::model::config::{EndpointTuple, ModelConfig, TierConfig};
use crate::model::scan::{scan_string, scan_value_end, skip_ws_comments};

/// Why a persistence attempt failed. Surfaced on the confirm line — a
/// failed save is loud, never silent.
#[derive(Debug)]
pub enum PersistError {
    /// No config file behind this store — the pick stays session-only.
    NoConfigFile,
    Read(std::io::Error),
    /// The file does not parse as `ModelConfig` — never splice blind.
    Parse(String),
    /// Parses, but the raw text has no top-level `main` member to anchor the
    /// insert on.
    NoMainKey,
    /// The surgery output does not re-parse — internal guard: refuse the
    /// write and leave the original file untouched (never land a splice we
    /// cannot read back).
    Reparse(String),
    Write(std::io::Error),
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PersistError::NoConfigFile => write!(f, "no config file found (pick is session-only)"),
            PersistError::Read(e) => write!(f, "cannot read config: {e}"),
            PersistError::Parse(e) => write!(f, "config does not parse: {e}"),
            PersistError::NoMainKey => write!(f, "no top-level `main` key in config"),
            PersistError::Reparse(e) => {
                write!(f, "internal guard: spliced config does not re-parse ({e})")
            }
            PersistError::Write(e) => write!(f, "cannot write config: {e}"),
        }
    }
}

impl std::error::Error for PersistError {}

/// Persist the picked main model as the `model` selection pointer in `path` —
/// the config file the store was loaded from. `None` (no config behind the
/// store) is a loud failure: the pick stays session-only and the confirm line
/// must say so. `runtime` is the endpoint the session is actually running
/// (the CLI-merged top-level defaults) — never persisted even when the pick
/// carries it (see `render_pointer_value`). The caller reports the outcome
/// via [`note`].
pub fn persist_to(
    path: Option<&Path>,
    picked: &EndpointTuple,
    runtime: (&str, &str, &str),
) -> Result<(), PersistError> {
    let path = path.ok_or(PersistError::NoConfigFile)?;
    set_pointer_at(path, picked, runtime)
}

/// Clear the selection pointer (picker「clear selection」): delete the
/// top-level `model` member so the next boot runs the configured `main`
/// again. A file without the member is already clear — untouched, success.
/// `None` (no config behind the store) is the same loud failure as
/// [`persist_to`].
pub fn clear_selection(path: Option<&Path>) -> Result<(), PersistError> {
    let path = path.ok_or(PersistError::NoConfigFile)?;
    clear_selection_at(path)
}

/// The confirm-line note for a persistence attempt: empty when the pick
/// changed nothing, a save marker on success, a loud line on failure.
/// Both markers land on their own line — glued behind a diff line the
/// save marker reads as a property of the diff, not of the write.
/// `deferred` appends the honest engine-timing line: the pick and the
/// save are live now, but a streaming turn keeps the old provider until
/// it lands (the swap is staged, not hot).
pub fn note(unchanged: bool, result: Result<(), PersistError>, deferred: bool) -> String {
    if unchanged {
        return String::new();
    }
    let mut out = match result {
        Ok(()) => "\n  saved to config.json".to_string(),
        Err(e) => format!("\n! config.json not saved: {e}"),
    };
    if deferred {
        out.push_str("\n  applies after the current turn");
    }
    out
}

/// Splice the picked model into the `model` pointer of `path` (surgery 1 or
/// 2), re-parse-guarded and written atomically.
pub fn set_pointer_at(
    path: &Path,
    picked: &EndpointTuple,
    runtime: (&str, &str, &str),
) -> Result<(), PersistError> {
    let raw = std::fs::read_to_string(path).map_err(PersistError::Read)?;
    let cfg: ModelConfig = json5::from_str(&raw).map_err(|e| PersistError::Parse(e.to_string()))?;
    let value = render_pointer_value(&cfg, runtime, picked);
    let updated = splice_pointer(&raw, &value)?;
    write_guarded(path, &updated)
}

/// Delete the `model` pointer member of `path` (surgery 3), re-parse-guarded
/// and written atomically. No member present = already clear: the file is
/// not touched.
pub fn clear_selection_at(path: &Path) -> Result<(), PersistError> {
    let raw = std::fs::read_to_string(path).map_err(PersistError::Read)?;
    json5::from_str::<ModelConfig>(&raw).map_err(|e| PersistError::Parse(e.to_string()))?;
    let updated = delete_pointer(&raw);
    if updated == raw {
        return Ok(());
    }
    write_guarded(path, &updated)
}

/// The selection pointer a config file carries, resolved the way a boot
/// would (top-level defaults + the pointer's own overrides): `None` when
/// there is no file, it does not parse, or it carries no `model` pointer.
/// 「Clear selection」uses this to tell "the live selection is this file's
/// pointer" (delete it) apart from "the live selection came from elsewhere"
/// (a CLI `--model`; the file's pointer is somebody else's and survives).
pub fn pointer_tuple_at(path: Option<&Path>) -> Option<EndpointTuple> {
    let path = path?;
    let raw = std::fs::read_to_string(path).ok()?;
    let cfg: ModelConfig = json5::from_str(&raw).ok()?;
    cfg.model.as_ref()?;
    cfg.tier_config("main")
}

/// Whether the file's `model` pointer IS `live`: every field the pointer
/// DECLARES matches the live tuple (a `Simple` pointer declares only the
/// name; a `Full` one also declares its endpoint overrides). `None` = no
/// file / no parse / no pointer. Undeclared endpoint fields do not
/// constrain — they are inherited at resolution, and a CLI override may
/// legitimately move them without changing which selection is live.
pub fn file_pointer_is_selection(path: Option<&Path>, live: &EndpointTuple) -> Option<bool> {
    let path = path?;
    let raw = std::fs::read_to_string(path).ok()?;
    let cfg: ModelConfig = json5::from_str(&raw).ok()?;
    let pointer = cfg.model.as_ref()?;
    let (model, url, key, proto) = live;
    let name_matches = pointer.model() == model.as_str();
    Some(match pointer {
        TierConfig::Simple(_) => name_matches,
        TierConfig::Full {
            base_url,
            api_key,
            protocol,
            ..
        } => {
            name_matches
                && base_url.as_deref().is_none_or(|v| v == url)
                && api_key.as_deref().is_none_or(|v| v == key)
                && protocol.as_deref().is_none_or(|v| v == proto)
        }
    })
}

/// Re-parse the surgery output — a splice that does not parse back is
/// refused (R1: the original file must survive a bug in here) — then replace
/// `path` atomically. Shared with `phimint config set` (config_write).
pub(crate) fn write_guarded(path: &Path, updated: &str) -> Result<(), PersistError> {
    json5::from_str::<ModelConfig>(updated).map_err(|e| PersistError::Reparse(e.to_string()))?;
    write_atomic(path, updated)
}

/// Replace `path` with `updated` atomically: write a tmp file (created 0600 —
/// it briefly holds the live API key), copy the original's permissions onto
/// it, then rename over the original. The tmp name carries the pid and a
/// counter: two writers (or two threads, as the 2026-11-07 test incident
/// proved) must never share one.
fn write_atomic(path: &Path, updated: &str) -> Result<(), PersistError> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("phimint-tmp-{}-{n}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp).map_err(PersistError::Write)?;
    std::io::Write::write_all(&mut file, updated.as_bytes()).map_err(PersistError::Write)?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(PersistError::Write(e));
    }
    Ok(())
}

/// Splice the `model` pointer value into `raw`: surgery 1 (replace the value
/// span) when the key is present, surgery 2 (insert the member) when not.
fn splice_pointer(raw: &str, value: &str) -> Result<String, PersistError> {
    set_member_text(raw, "model", value)
}

/// The generic surgery behind [`splice_pointer`] and `phimint config set`
/// (config_write): replace the `key` member's value span when the member is
/// present, else insert the member after `main`. `value` is the JSON text to
/// splice (already encoded — see [`jstr`]).
pub(crate) fn set_member_text(raw: &str, key: &str, value: &str) -> Result<String, PersistError> {
    match find_member_span(raw, key) {
        Some(span) => Ok(format!(
            "{}{}{}",
            &raw[..span.value_start],
            value,
            &raw[span.value_end..]
        )),
        None => insert_member(raw, key, value),
    }
}

/// Surgery 2: insert `"<key>": <value>` right after the top-level `main`
/// member. The member-to-member separator copies the whitespace before the
/// `main` key (multi-line files get newline + indent, single-line none) and
/// the key→colon and colon→value gaps copy `main`'s own, so the new member
/// reads like its siblings. Comma ownership is unique: the comma trails the
/// new member when it has a right neighbor (or `main` already ended in one —
/// that file's JSON5 trailing-comma style is preserved); when the new member
/// is last, its comma goes in front of it.
fn insert_member(raw: &str, key: &str, value: &str) -> Result<String, PersistError> {
    let main = find_member_span(raw, "main").ok_or(PersistError::NoMainKey)?;
    let b = raw.as_bytes();
    let mut sep_start = main.key_start;
    while sep_start > 0 && matches!(b[sep_start - 1], b' ' | b'\t' | b'\n' | b'\r') {
        sep_start -= 1;
    }
    let separator = &raw[sep_start..main.key_start];
    let member = format!(
        "\"{key}\"{}:{}{}",
        main.key_gap(raw),
        main.value_gap(raw),
        value
    );
    let after = skip_ws_comments(b, main.value_end);
    if b.get(after) == Some(&b',') {
        // Right neighbor (or trailing-comma style): insert right after
        // `main`'s own comma, the new member's comma trailing it.
        let at = after + 1;
        Ok(format!("{}{separator}{member},{}", &raw[..at], &raw[at..]))
    } else {
        // `main` is the last member: the comma goes before the new member.
        Ok(format!(
            "{},{separator}{member}{}",
            &raw[..main.value_end],
            &raw[main.value_end..]
        ))
    }
}

/// Surgery 3: delete the `model` member plus one adjacent comma — the
/// trailing comma and the whitespace-only gap after it when the member has a
/// right neighbor, else the comma in front of the member (plus the
/// whitespace-only gap after that comma). Comments around the member are
/// never rewritten: an orphan comment survives, and the gap is kept whenever
/// the orphan region carries a `//` comment (a line comment would otherwise
/// swallow the brace that lands after it).
fn delete_pointer(raw: &str) -> String {
    let Some(span) = find_member_span(raw, "model") else {
        return raw.to_string();
    };
    let b = raw.as_bytes();
    // The member itself: key start → value end.
    let mut cuts: Vec<(usize, usize)> = vec![(span.key_start, span.value_end)];
    let after = skip_ws_comments(b, span.value_end);
    if b.get(after) == Some(&b',') {
        // Trailing comma: take it and the whitespace-only gap after it.
        let mut end = after + 1;
        while end < b.len() && matches!(b[end], b' ' | b'\t' | b'\n' | b'\r') {
            end += 1;
        }
        cuts.push((after, end));
    } else if let Some(comma) = span.comma_before {
        // Last member: take the comma in front and the whitespace-only gap
        // between it and the key — unless the orphan region in between ends
        // in a line comment (see the doc comment).
        cuts.push((comma, comma + 1));
        let mut gap_start = span.key_start;
        while gap_start > 0 && matches!(b[gap_start - 1], b' ' | b'\t' | b'\n' | b'\r') {
            gap_start -= 1;
        }
        if !raw[comma + 1..gap_start].contains("//") {
            cuts.push((gap_start, span.key_start));
        }
    }
    cuts.sort_unstable();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    for (start, end) in cuts {
        out.push_str(&raw[i..start]);
        i = end;
    }
    out.push_str(&raw[i..]);
    out
}

/// The replacement `model` pointer value for `picked`: a bare model string
/// when nothing but the name differs, else a `Full` object carrying only the
/// endpoint fields the pick genuinely carries (delta semantics — resolution
/// is the endpoint chain of [`ModelConfig::tier_config`], own → top-level →
/// configured `main`, plus these overrides).
///
/// `runtime` is the endpoint the session is actually running (the
/// CLI-merged top-level defaults). A field lands in the pointer only when
/// the pick differs from BOTH the file's top-level default and the runtime
/// default — a field that merely mirrors a CLI override is session policy,
/// never file content; persisting it would turn a one-off endpoint/account
/// into a permanent spend. One pathological case is accepted and recorded
/// here: a picked field that genuinely differs from the file but happens to
/// equal the CLI value is left out and the pointer inherits the file's
/// field instead (it takes a hand-placed override colliding with a picked
/// row's own field).
fn render_pointer_value(
    cfg: &ModelConfig,
    runtime: (&str, &str, &str),
    picked: &EndpointTuple,
) -> String {
    let file_url = cfg.base_url.clone().unwrap_or_default();
    let file_key = cfg.api_key.clone().unwrap_or_default();
    let file_proto = cfg.protocol.clone().unwrap_or_default();
    let (model, url, key, proto) = picked;
    let (run_url, run_key, run_proto) = runtime;

    let emit_url = url != &file_url && url != run_url;
    let emit_key = key != &file_key && key != run_key;
    let emit_proto = proto != &file_proto && proto != run_proto;
    if !emit_url && !emit_key && !emit_proto {
        return jstr(model);
    }
    let mut out = format!("{{\"model\":{}", jstr(model));
    if emit_url {
        out.push_str(&format!(",\"base_url\":{}", jstr(url)));
    }
    if emit_key {
        out.push_str(&format!(",\"api_key\":{}", jstr(key)));
    }
    if emit_proto {
        out.push_str(&format!(",\"protocol\":{}", jstr(proto)));
    }
    out.push('}');
    out
}

/// JSON string literal for `s` (serialization of a string is infallible) —
/// the `value` half of a member surgery. Shared with config_write.
pub(crate) fn jstr(s: &str) -> String {
    serde_json::to_string(s).expect("JSON string serialization is infallible")
}

/// One top-level member located in the raw config text.
struct MemberSpan {
    /// Start of the key: the opening quote of a quoted key, or the first
    /// identifier character of a bare one.
    key_start: usize,
    /// End of the key (closing quote + 1, or one past the identifier).
    key_end: usize,
    /// Index of the `:` between key and value.
    colon: usize,
    /// Start of the value (quotes or braces included).
    value_start: usize,
    /// End of the value (exclusive).
    value_end: usize,
    /// Position of the last top-level `,` before `key_start`, if any — the
    /// member-separator comma on the preceding side (delete surgery).
    comma_before: Option<usize>,
}

impl MemberSpan {
    /// The file's own key→colon whitespace — an inserted sibling copies it,
    /// so `"main" : "x"` yields `"model" : "y"`.
    fn key_gap<'a>(&self, raw: &'a str) -> &'a str {
        &raw[self.key_end..self.colon]
    }

    /// The file's own key→value whitespace — an inserted sibling copies it,
    /// so `"main": "x"` yields `"model": "y"` and `{"main":"x"}` yields
    /// `"model":"y"`.
    fn value_gap<'a>(&self, raw: &'a str) -> &'a str {
        &raw[self.colon + 1..self.value_start]
    }
}

/// Locate the member named `key` in a JSON5 config text (quotes or braces of
/// the value included in the spans). Only a key at object depth 1 counts, so
/// a look-alike nested inside another section is never touched. Comments are
/// skipped, never interpreted.
fn find_member_span(raw: &str, key: &str) -> Option<MemberSpan> {
    let b = raw.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut comma_before: Option<usize> = None;
    while i < b.len() {
        match b[i] {
            b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            b'"' | b'\'' => {
                let end = scan_string(b, i)?;
                if depth == 1 && &raw[i + 1..end - 1] == key {
                    let after = skip_ws_comments(b, end);
                    if b.get(after) == Some(&b':') {
                        let value_start = skip_ws_comments(b, after + 1);
                        let value_end = scan_value_end(b, value_start)?;
                        return Some(MemberSpan {
                            key_start: i,
                            key_end: end,
                            colon: after,
                            value_start,
                            value_end,
                            comma_before,
                        });
                    }
                }
                i = end;
            }
            c if depth == 1 && (c.is_ascii_alphabetic() || c == b'_' || c == b'$') => {
                let mut e = i;
                while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_' || b[e] == b'$')
                {
                    e += 1;
                }
                if &raw[i..e] == key {
                    let after = skip_ws_comments(b, e);
                    if b.get(after) == Some(&b':') {
                        let value_start = skip_ws_comments(b, after + 1);
                        let value_end = scan_value_end(b, value_start)?;
                        return Some(MemberSpan {
                            key_start: i,
                            key_end: e,
                            colon: after,
                            value_start,
                            value_end,
                            comma_before,
                        });
                    }
                }
                i = e;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b',' if depth == 1 => {
                comma_before = Some(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(model: &str, url: &str, key: &str, proto: &str) -> EndpointTuple {
        (
            model.to_string(),
            url.to_string(),
            key.to_string(),
            proto.to_string(),
        )
    }

    fn cfg(default_url: &str, default_key: &str, default_proto: &str) -> ModelConfig {
        json5::from_str(&format!(
            r#"{{"main": "boot-model", "base_url": "{}", "api_key": "{}", "protocol": "{}"}}"#,
            default_url, default_key, default_proto
        ))
        .unwrap()
    }

    /// protects: surgery 1 replaces only the `model` value span — comments,
    /// sibling keys, tier definitions and a `model` nested in another section
    /// survive byte-for-byte. fails_when: any byte outside the value span
    /// changes, or a nested look-alike is touched. no_claim: what a config
    /// file may otherwise contain.
    #[test]
    fn the_pointer_value_is_replaced_and_everything_else_survives() {
        let raw = "{\n  // endpoint for everything\n  \"base_url\": \"https://api.example.com\",\n  \"main\": \"boot-model\",\n  \"model\": \"old-pick\", // the pick\n  \"ui\": {\"model\": \"nested-untouched\"}\n}\n";
        let updated = splice_pointer(raw, "\"picked-model\"").unwrap();
        assert_eq!(
            updated,
            "{\n  // endpoint for everything\n  \"base_url\": \"https://api.example.com\",\n  \"main\": \"boot-model\",\n  \"model\": \"picked-model\", // the pick\n  \"ui\": {\"model\": \"nested-untouched\"}\n}\n",
            "only the value span moves: {updated}"
        );

        // JSON5 forms: single-quoted value, bare key, comments around.
        let raw = "{ /* c */ model /* k */ : 'old' , other: 1 }";
        assert_eq!(
            splice_pointer(raw, "\"p\"").unwrap(),
            "{ /* c */ model /* k */ : \"p\" , other: 1 }",
            "JSON5 key and comment forms are handled"
        );

        // An object value replaced whole.
        let raw = "{\"model\": {\"model\": \"old\", \"api_key\": \"sk\"}, \"z\": 0}";
        assert_eq!(
            splice_pointer(raw, "\"new\"").unwrap(),
            "{\"model\": \"new\", \"z\": 0}",
            "object value replaced"
        );
    }

    /// protects: surgery 2 inserts after the `main` member with unique comma
    /// ownership on both branches — the new member's separator and key→value
    /// gap copy the file's own style. fails_when: a missing or stray comma
    /// corrupts the file, or a single-line file grows whitespace. no_claim:
    /// duplicate top-level keys.
    #[test]
    fn an_absent_pointer_is_inserted_after_the_main_member() {
        // Multi-line, `main` has a right neighbor: comma trails the new member.
        let raw = "{\n  \"main\": \"mimo-v2.6-pro\",\n  \"lite\": \"mimo-v2.5\"\n}\n";
        assert_eq!(
            insert_member(raw, "model", "\"MiniMax-M3.1-Flash\"").unwrap(),
            "{\n  \"main\": \"mimo-v2.6-pro\",\n  \"model\": \"MiniMax-M3.1-Flash\",\n  \"lite\": \"mimo-v2.5\"\n}\n"
        );

        // Multi-line, `main` is the last member: comma goes before the new one.
        let raw = "{\n  \"main\": \"alpha\",\n  \"api_key\": \"sk\"\n}\n";
        // (api_key after main: anchor is the `main` MEMBER, so the pointer
        // lands right after it, before api_key.)
        assert_eq!(
            insert_member(raw, "model", "\"beta\"").unwrap(),
            "{\n  \"main\": \"alpha\",\n  \"model\": \"beta\",\n  \"api_key\": \"sk\"\n}\n"
        );
        let raw = "{\n  \"api_key\": \"sk\",\n  \"main\": \"alpha\"\n}\n";
        assert_eq!(
            insert_member(raw, "model", "\"beta\"").unwrap(),
            "{\n  \"api_key\": \"sk\",\n  \"main\": \"alpha\",\n  \"model\": \"beta\"\n}\n"
        );

        // Single-line fixtures (design samples, byte-exact).
        assert_eq!(
            insert_member("{\"main\":\"x\"}", "model", "\"y\"").unwrap(),
            "{\"main\":\"x\",\"model\":\"y\"}"
        );
        assert_eq!(
            insert_member("{\"main\":\"x\",\"lite\":\"z\"}", "model", "\"y\"").unwrap(),
            "{\"main\":\"x\",\"model\":\"y\",\"lite\":\"z\"}"
        );

        // JSON5 trailing-comma style is preserved (branch 1).
        assert_eq!(
            insert_member("{\"main\":\"x\",}", "model", "\"y\"").unwrap(),
            "{\"main\":\"x\",\"model\":\"y\",}"
        );

        // The key→value gap copies `main`'s own: compact stays compact.
        assert_eq!(
            insert_member("{\n  \"main\" : \"x\"\n}", "model", "\"y\"").unwrap(),
            "{\n  \"main\" : \"x\",\n  \"model\" : \"y\"\n}"
        );

        // No `main` to anchor on: loud, nothing written.
        assert!(matches!(
            insert_member("{\"base_url\": \"u\"}", "model", "\"y\""),
            Err(PersistError::NoMainKey)
        ));
    }

    /// protects: surgery 3 deletes the member plus exactly one adjacent
    /// comma and leaves the file parseable — orphan comments survive and a
    /// line comment never swallows the closing brace. fails_when: a dangling
    /// comma remains, a comment byte is rewritten, or the output stops
    /// parsing. no_claim: concurrent deletes from two instances.
    #[test]
    fn the_pointer_member_deletes_with_one_adjacent_comma() {
        // Middle member: trailing comma and the gap after it go too.
        assert_eq!(
            delete_pointer("{\n  \"main\": \"x\",\n  \"model\": \"y\",\n  \"lite\": \"z\"\n}"),
            "{\n  \"main\": \"x\",\n  \"lite\": \"z\"\n}"
        );
        // Last member: the comma in front and the gap after it go too.
        assert_eq!(
            delete_pointer("{\n  \"main\": \"x\",\n  \"model\": \"y\"\n}"),
            "{\n  \"main\": \"x\"\n}"
        );
        assert_eq!(
            delete_pointer("{\"main\":\"x\",\"model\":\"y\"}"),
            "{\"main\":\"x\"}"
        );
        // Orphan comments survive on both sides.
        assert_eq!(
            delete_pointer("{\"main\":\"x\", /* c */ \"model\":\"y\"}"),
            "{\"main\":\"x\" /* c */}"
        );
        assert_eq!(
            delete_pointer("{\"model\":\"y\" /* c */, \"lite\":\"z\"}"),
            "{ /* c */\"lite\":\"z\"}",
            "the orphan comment survives; the trailing comma and its gap go"
        );
        // A line comment before the last member keeps its newline gap — the
        // closing brace must not land on the comment's line (the adjacent
        // comma still goes, per the surgery contract).
        assert_eq!(
            delete_pointer("{\"main\":\"x\", // note\n  \"model\":\"y\"}"),
            "{\"main\":\"x\" // note\n  }"
        );
        // Nested look-alikes and an absent key: untouched.
        assert_eq!(
            delete_pointer("{\"main\":{\"model\":\"nested\"}}"),
            "{\"main\":{\"model\":\"nested\"}}"
        );
    }

    /// protects: the rendered pointer value carries exactly the endpoint
    /// facts the pick needs and the file has never seen — a bare name when
    /// nothing but the name differs, otherwise a `Full` object with only the
    /// genuinely differing fields. A field that merely mirrors the runtime's
    /// CLI-merged endpoint is session policy and is NEVER persisted.
    /// fails_when: a mirrored CLI field lands in the pointer, a genuine
    /// delta is dropped, or a field equal to the file default is written.
    /// no_claim: config keys outside `model`.
    #[test]
    fn the_pointer_value_rides_the_picked_endpoint() {
        let cfg = cfg("https://api.example.com", "sk-default", "anthropic");
        let runtime = ("https://api.example.com", "sk-default", "anthropic");

        // Same endpoint as the defaults: the name alone is enough.
        assert_eq!(
            render_pointer_value(
                &cfg,
                runtime,
                &tuple(
                    "picked",
                    "https://api.example.com",
                    "sk-default",
                    "anthropic"
                )
            ),
            "\"picked\""
        );

        // A different endpoint: only the differing fields come along.
        assert_eq!(
            render_pointer_value(
                &cfg,
                runtime,
                &tuple(
                    "picked",
                    "https://other.example.com",
                    "sk-default",
                    "anthropic"
                )
            ),
            "{\"model\":\"picked\",\"base_url\":\"https://other.example.com\"}"
        );
        assert_eq!(
            render_pointer_value(
                &cfg,
                runtime,
                &tuple("picked", "https://other.example.com", "sk-x", "openai")
            ),
            "{\"model\":\"picked\",\"base_url\":\"https://other.example.com\",\"api_key\":\"sk-x\",\"protocol\":\"openai\"}"
        );

        // The runtime endpoint (a CLI override) is never persisted: fields
        // that merely mirror it stay out of the pointer even when they
        // differ from the file.
        assert_eq!(
            render_pointer_value(
                &cfg,
                ("https://cli.example.com", "sk-cli", "openai"),
                &tuple("picked", "https://cli.example.com", "sk-cli", "anthropic")
            ),
            "\"picked\"",
            "the CLI base_url/api_key mirror is session policy, not file content"
        );

        // Mixed: a genuine pick delta survives while the mirrored field
        // stays out.
        assert_eq!(
            render_pointer_value(
                &cfg,
                ("https://cli.example.com", "sk-default", "anthropic"),
                &tuple(
                    "picked",
                    "https://row.example.com",
                    "sk-default",
                    "anthropic"
                )
            ),
            "{\"model\":\"picked\",\"base_url\":\"https://row.example.com\"}"
        );
    }

    /// protects: an unparseable config fails loudly and leaves the file
    /// untouched; a good file round-trips through two picks with comments
    /// intact and the tier definitions byte-identical (the「model list
    /// shrinks」data-loss regression at unit scale). fails_when: a failed
    /// attempt mutates the file, or any tier definition's bytes move.
    /// no_claim: crash-durability beyond the rename window.
    #[test]
    fn a_failed_save_never_touches_the_file_and_a_good_one_round_trips() {
        let dir = std::env::temp_dir().join(format!("phimint-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // Unparseable file: loud error, bytes untouched.
        let bad = "{ this is not json ";
        std::fs::write(&path, bad).unwrap();
        assert!(matches!(
            set_pointer_at(&path, &tuple("p", "u", "k", "o"), ("u", "k", "o")),
            Err(PersistError::Parse(_))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);

        // A good file: two picks, comments and tier definitions survive, the
        // pointer value tracks the pick.
        let raw = "{\n  // keep me\n  \"main\": \"boot\",\n  \"lite\": \"lite-model\",\n  \"base_url\": \"u\",\n  \"api_key\": \"k\",\n  \"protocol\": \"o\"\n}\n";
        std::fs::write(&path, raw).unwrap();
        set_pointer_at(&path, &tuple("first", "u", "k", "o"), ("u", "k", "o")).unwrap();
        let after_one = std::fs::read_to_string(&path).unwrap();
        set_pointer_at(&path, &tuple("second", "u2", "k", "o"), ("u", "k", "o")).unwrap();
        let after_two = std::fs::read_to_string(&path).unwrap();
        for (label, text) in [("first", &after_one), ("second", &after_two)] {
            assert!(text.contains("// keep me"), "{label}: {text}");
            for definition in ["\"main\": \"boot\"", "\"lite\": \"lite-model\""] {
                assert!(
                    text.contains(definition),
                    "{label}: tier definition moved: {text}"
                );
            }
        }
        let parsed: ModelConfig = json5::from_str(&after_two).unwrap();
        let (model, url, key, proto) = parsed.tier_config("main").unwrap();
        assert_eq!(model, "second");
        assert_eq!(
            url, "u2",
            "the picked endpoint travels in the `model` pointer"
        );
        assert_eq!(key, "k");
        assert_eq!(proto, "o");
        assert_eq!(
            parsed.main.model(),
            "boot",
            "the configured `main` definition is never rewritten"
        );
        assert_eq!(
            parsed.base_url.as_deref(),
            Some("u"),
            "top-level defaults untouched"
        );

        // No config behind the store: loud failure, no file involved.
        assert!(matches!(
            persist_to(None, &tuple("p", "u", "k", "o"), ("u", "k", "o")),
            Err(PersistError::NoConfigFile)
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// protects: the re-parse guard refuses a splice that does not parse
    /// back and the original file survives byte-for-byte (R1 — a corrupt
    /// splice must never land). fails_when: an invalid surgery output is
    /// written anyway. no_claim: the surgery functions producing such an
    /// output (this exercises the guard itself).
    #[test]
    fn a_surgery_that_does_not_reparse_is_refused_and_the_file_survives() {
        let dir = std::env::temp_dir().join(format!("phimint-reparse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let raw = "{\n  \"main\": \"boot\"\n}\n";
        std::fs::write(&path, raw).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = write_guarded(&path, "{ this is not json ").unwrap_err();
        assert!(matches!(err, PersistError::Reparse(_)), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before, "file untouched");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// protects: concurrent writers never tear the file — unique tmp names
    /// (pid + counter) and atomic rename mean the config always parses and
    /// holds one writer's whole value. fails_when: two writers share a tmp
    /// name and interleave bytes into corrupt JSON. no_claim: which writer
    /// wins (last-writer-wins is the policy).
    #[test]
    fn two_concurrent_writers_never_tear_the_file() {
        let dir = std::env::temp_dir().join(format!("phimint-concurrent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{\n  \"main\": \"boot\"\n}\n").unwrap();

        let a = {
            let path = path.clone();
            std::thread::spawn(move || {
                set_pointer_at(&path, &tuple("writer-a", "u", "k", "o"), ("u", "k", "o"))
            })
        };
        let b = {
            let path = path.clone();
            std::thread::spawn(move || {
                set_pointer_at(&path, &tuple("writer-b", "u", "k", "o"), ("u", "k", "o"))
            })
        };
        a.join().unwrap().unwrap();
        b.join().unwrap().unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: ModelConfig = json5::from_str(&raw).unwrap();
        let model = parsed.tier_config("main").unwrap().0;
        assert!(
            model == "writer-a" || model == "writer-b",
            "one writer's whole value, never a mix: {raw}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// protects: clearing a selection from a file that has no `model` member
    /// is already-clear, not a failure — the file is left byte-identical and
    /// the call succeeds. fails_when: a clear over a pristine file errors
    /// loudly, or rewrites the file (the API key's bytes move for nothing).
    /// no_claim: what a clear does when a member IS present (the delete
    /// surgery's own card).
    #[test]
    fn clearing_a_file_without_a_pointer_is_a_no_op() {
        let dir = std::env::temp_dir().join(format!("phimint-clear-nokey-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let raw = "{\n  // keep me\n  \"main\": \"boot\",\n  \"api_key\": \"sk-live\"\n}\n";
        std::fs::write(&path, raw).unwrap();

        assert!(clear_selection_at(&path).is_ok());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            raw,
            "an already-clear file is not rewritten"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// protects: the confirm note says nothing when nothing changed, marks
    /// a save, and shouts a failure. fails_when: a failed save reads as
    /// success. no_claim: the surrounding confirm copy.
    #[test]
    fn the_note_reports_the_save_outcome() {
        assert_eq!(note(true, Ok(()), false), "");
        assert_eq!(note(false, Ok(()), false), "\n  saved to config.json");
        assert_eq!(
            note(false, Err(PersistError::NoConfigFile), false),
            "\n! config.json not saved: no config file found (pick is session-only)"
        );
    }

    /// protects: when the engine staged the swap behind a streaming turn,
    /// the note says the pick applies after that turn. fails_when: the
    /// confirm line implies the new model already answers while the old
    /// provider still owns the run. no_claim: engine internals.
    #[test]
    fn the_note_marks_a_deferred_swap() {
        assert_eq!(
            note(false, Ok(()), true),
            "\n  saved to config.json\n  applies after the current turn"
        );
    }

    /// protects: the atomic write's tmp is created 0600 — the file briefly
    /// holds the live API key, so its first bytes on disk are never
    /// group/other accessible (with no prior file to copy permissions from,
    /// the tmp's creation mode IS the final mode). fails_when: the tmp is
    /// created with any group/other bit (dropping `opts.mode(0o600)` turns
    /// this red). no_claim: the mode of a file that already exists — the
    /// original's permissions are copied back onto it by design (unix-only:
    /// Windows has no mode bits).
    #[cfg(unix)]
    #[test]
    fn a_first_write_lands_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("phimint-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        // No prior file: no permission copy-back runs, so the on-disk mode
        // is the tmp creation mode.
        write_atomic(
            &path,
            "{\n  \"main\": \"boot\",\n  \"api_key\": \"sk-live\"\n}\n",
        )
        .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "the config (holding the API key) is never group/other accessible: {mode:o}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// protects: the file's own pointer resolves the way a boot would — no
    /// pointer, no answer; a bare pointer inherits the top-level defaults; a
    /// `Full` pointer resolves whole (its overrides + the defaults).
    /// fails_when: `pointer_tuple_at` reports a pointer for a file without
    /// one, or drops a pointer's own endpoint overrides. no_claim: what a
    /// clear does with the answer (the command layer's card).
    #[test]
    fn the_file_pointer_resolves_like_a_boot_would() {
        let dir = std::env::temp_dir().join(format!("phimint-ptr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let bare = "{\n  \"main\": \"boot\",\n  \"base_url\": \"u\",\n  \"api_key\": \"k\",\n  \"protocol\": \"o\"\n}\n";
        std::fs::write(&path, bare).unwrap();
        assert_eq!(pointer_tuple_at(Some(&path)), None, "no pointer, no answer");

        let with_pointer = "{\n  \"main\": \"boot\",\n  \"base_url\": \"u\",\n  \"api_key\": \"k\",\n  \"protocol\": \"o\",\n  \"model\": \"picked\"\n}\n";
        std::fs::write(&path, with_pointer).unwrap();
        assert_eq!(
            pointer_tuple_at(Some(&path)),
            Some(tuple("picked", "u", "k", "o")),
            "a bare pointer inherits the top-level defaults"
        );

        let full_pointer = "{\n  \"main\": \"boot\",\n  \"base_url\": \"u\",\n  \"api_key\": \"k\",\n  \"protocol\": \"o\",\n  \"model\": {\"model\": \"picked\", \"base_url\": \"row\"}\n}\n";
        std::fs::write(&path, full_pointer).unwrap();
        assert_eq!(
            pointer_tuple_at(Some(&path)),
            Some(tuple("picked", "row", "k", "o")),
            "a Full pointer resolves whole: its overrides + the defaults"
        );

        assert_eq!(pointer_tuple_at(None), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
