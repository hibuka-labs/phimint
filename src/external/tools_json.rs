//! `tools.json` — the global script-tool surface (human + agent-written
//! (human-confirmed)).
//!
//! One line = one [`ScriptToolEntry`] (the framework's serde shape is the
//! entry shape, see `agent-works/src/script_tool.rs`): name law + path law
//! already pass at parse time (fail-closed). Two file shapes are accepted:
//! `{ "tools": [ … ] }` or a bare array (hand-written files come both ways).
//! The product layer adds three rules:
//!
//! - **whole-file syntax parse failure → load no entries at all + a decent
//!   error message** (design wording);
//! - **duplicate names: the later one (file order) is skipped at load time
//!   with a notice**; bad entries are likewise skipped one by one + noticed;
//! - `${ENV:}` pre-resolution happens once, at **tool registration** (for
//!   `env` values); a missing variable reports its name.
//!
//! Deletion goes through [`remove_entry`]: write back + ledger cleanup of
//! `script.{name}` (symmetric with `plugin remove`, no orphaned memory) — the
//! ledger cleanup is done by the caller (`/tools`).

use std::path::Path;

use phi_agent::script_tool::ScriptToolEntry;
use serde::Serialize;

/// tools.json file-level / entry-level errors.
#[derive(Debug)]
pub enum ToolsFileError {
    Io(std::io::Error),
    Parse(String),
    BadEntry { index: usize, reason: String },
    Duplicate { name: String },
    NotFound { name: String },
}

impl std::fmt::Display for ToolsFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read tools.json: {e}"),
            Self::Parse(e) => {
                write!(f, "tools.json is not valid — no tools loaded ({e})")
            }
            Self::BadEntry { index, reason } => {
                write!(f, "tools.json entry #{index} rejected: {reason}")
            }
            Self::Duplicate { name } => write!(f, "tool `{name}` already exists"),
            Self::NotFound { name } => write!(f, "tool `{name}` not found"),
        }
    }
}

impl std::error::Error for ToolsFileError {}

/// Load report: valid entries + per-item notices (duplicates/bad entries do
/// not fail the whole file).
#[derive(Debug, Default)]
pub struct LoadReport {
    pub entries: Vec<ScriptToolEntry>,
    /// Decent error notices (one message on a file-level error; one line per
    /// skipped entry).
    pub notices: Vec<String>,
}

/// Load tools.json. Missing file = empty table.
///
/// Whole-file syntax error = load none (design: "load no entries at all + a
/// decent error message"); entry-level badness (name law/path law fails) =
/// skip that entry + notice, load the rest.
pub fn load(path: &Path) -> LoadReport {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LoadReport::default(),
        Err(e) => {
            return LoadReport {
                entries: Vec::new(),
                notices: vec![ToolsFileError::Io(e).to_string()],
            };
        }
    };
    let values: Vec<serde_json::Value> = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Array(values)) => values,
        Ok(serde_json::Value::Object(map)) => match map.get("tools") {
            Some(serde_json::Value::Array(values)) => values.clone(),
            _ => Vec::new(),
        },
        Ok(_) => Vec::new(),
        Err(e) => {
            return LoadReport {
                entries: Vec::new(),
                notices: vec![ToolsFileError::Parse(e.to_string()).to_string()],
            };
        }
    };
    let mut report = LoadReport::default();
    let mut seen = std::collections::BTreeSet::new();
    for (index, value) in values.into_iter().enumerate() {
        match serde_json::from_value::<ScriptToolEntry>(value) {
            Ok(entry) => {
                if seen.insert(entry.name.clone()) {
                    report.entries.push(entry);
                } else {
                    report.notices.push(
                        ToolsFileError::Duplicate {
                            name: entry.name.clone(),
                        }
                        .to_string(),
                    );
                }
            }
            Err(e) => {
                report.notices.push(
                    ToolsFileError::BadEntry {
                        index,
                        reason: e.to_string(),
                    }
                    .to_string(),
                );
            }
        }
    }
    report
}

/// Write tools.json back (`{ "tools": [ … ] }` pure-JSON roundtrip). An empty
/// table also writes `[]`.
pub fn save(path: &Path, entries: &[ScriptToolEntry]) -> Result<(), ToolsFileError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ToolsFileError::Io)?;
    }
    #[derive(Serialize)]
    struct File<'a> {
        tools: &'a [ScriptToolEntry],
    }
    let text = serde_json::to_string_pretty(&File { tools: entries })
        .map_err(|e| ToolsFileError::Parse(e.to_string()))?;
    std::fs::write(path, text).map_err(ToolsFileError::Io)
}

/// Append one entry (duplicates rejected). Does not persist — the caller
/// [`save`]s only after confirmation.
pub fn insert(
    entries: &mut Vec<ScriptToolEntry>,
    entry: ScriptToolEntry,
) -> Result<(), ToolsFileError> {
    if entries.iter().any(|e| e.name == entry.name) {
        return Err(ToolsFileError::Duplicate {
            name: entry.name.clone(),
        });
    }
    entries.push(entry);
    Ok(())
}

/// Remove one entry (entry name — the caller strips the `script.` prefix
/// from `/tools remove <key>`). Returns the removed entry; the caller cleans
/// the ledger.
pub fn remove_entry(
    entries: &mut Vec<ScriptToolEntry>,
    name: &str,
) -> Result<ScriptToolEntry, ToolsFileError> {
    let idx =
        entries
            .iter()
            .position(|e| e.name == name)
            .ok_or_else(|| ToolsFileError::NotFound {
                name: name.to_string(),
            })?;
    Ok(entries.remove(idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, run: Vec<String>) -> ScriptToolEntry {
        ScriptToolEntry::new(name, "description", run).unwrap()
    }

    #[test]
    fn file_shape_is_object_with_tools_array() {
        let path = std::env::temp_dir().join(format!("tools-{}.json", std::process::id()));
        let entries = vec![entry("deploy", vec!["/bin/true".into()])];
        save(&path, &entries).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"tools\""), "file form: {raw}");
        let report = load(&path);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].name, "deploy");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bare_array_is_accepted_too() {
        let path = std::env::temp_dir().join(format!("tools-bare-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"[{"name":"deploy","description":"d","run":["/bin/true"]}]"#,
        )
        .unwrap();
        let report = load(&path);
        assert_eq!(report.entries.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn whole_file_syntax_error_loads_nothing() {
        let path = std::env::temp_dir().join(format!("tools-bad-{}.json", std::process::id()));
        std::fs::write(&path, "not json at all").unwrap();
        let report = load(&path);
        assert!(report.entries.is_empty());
        assert_eq!(report.notices.len(), 1);
        assert!(report.notices[0].contains("no tools loaded"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn bad_entry_and_duplicate_are_skipped_with_notice() {
        let path = std::env::temp_dir().join(format!("tools-skip-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"tools":[
                {"name":"deploy","description":"d","run":["/bin/true"]},
                {"name":"deploy","description":"dup","run":["/bin/true"]},
                {"name":"relative","description":"bad path","run":["./not-absolute"]},
                {"name":"ok","description":"d","run":["/bin/true"]}
            ]}"#,
        )
        .unwrap();
        let report = load(&path);
        let names: Vec<&str> = report.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["deploy", "ok"],
            "dup skipped, bad entry skipped"
        );
        assert_eq!(report.notices.len(), 2, "{:?}", report.notices);
        assert!(report.notices[0].contains("already exists"));
        assert!(report.notices[1].contains("rejected"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let mut entries = Vec::new();
        insert(&mut entries, entry("deploy", vec!["/bin/true".into()])).unwrap();
        assert!(matches!(
            insert(&mut entries, entry("deploy", vec!["/bin/true".into()])),
            Err(ToolsFileError::Duplicate { .. })
        ));
        let removed = remove_entry(&mut entries, "deploy").unwrap();
        assert_eq!(removed.name, "deploy");
        assert!(matches!(
            remove_entry(&mut entries, "deploy"),
            Err(ToolsFileError::NotFound { .. })
        ));
    }
}
