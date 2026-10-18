//! Built-in connection catalog ("one command hooks up one ecosystem" for
//! `/connect <name>`).
//!
//! 8 built-in entries (design asks for 5–10), data in `catalog.json`
//! (embedded in the release artifact via include_str!). Each entry carries
//! its `${ENV:}` references and a `required_env` list — the UI uses the
//! latter to render "[env ready] / [missing X] — export …".

use serde::Deserialize;

use super::connections::{ConnectionEntry, TransportSpec};

/// Catalog JSON entry shape (`required_env` is the UI criterion;
/// `transport` is the connection template).
#[derive(Debug, Clone, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub required_env: Vec<String>,
    pub transport: TransportSpec,
}

#[derive(Debug, Deserialize)]
struct CatalogFile {
    entries: Vec<CatalogEntry>,
}

/// Built-in catalog (read-only in-process; parses one small JSON per call —
/// negligible cost).
pub fn builtin() -> Vec<CatalogEntry> {
    // Embedded in the release artifact: no path issues after packaging, and
    // the version travels with the binary.
    const RAW: &str = include_str!("catalog.json");
    let file: CatalogFile = serde_json::from_str(RAW).expect("built-in catalog.json is valid");
    file.entries
}

/// Find an entry by name (exact match on `name`).
pub fn lookup(name: &str) -> Option<CatalogEntry> {
    builtin().into_iter().find(|e| e.name == name)
}

impl CatalogEntry {
    /// Catalog entry → connection to be persisted (`enabled: true`,
    /// `${ENV:}` references preserved as-is).
    pub fn to_connection(&self) -> ConnectionEntry {
        ConnectionEntry {
            name: self.name.clone(),
            enabled: true,
            transport: self.transport.clone(),
        }
    }

    /// Environment-variable readiness criterion: all set → `Ok(())`;
    /// which are missing → `Err(list of missing)`.
    pub fn check_env<F>(&self, lookup_var: F) -> Result<(), Vec<String>>
    where
        F: Fn(&str) -> Option<String>,
    {
        let missing: Vec<String> = self
            .required_env
            .iter()
            .filter(|name| lookup_var(name).is_none())
            .cloned()
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(missing)
        }
    }
}

/// Catalog overview for `/connect` with no arguments (product copy).
pub fn render_catalog() -> String {
    let entries = builtin();
    let mut out = String::from("connect catalog — `/connect <name>`:\n");
    for e in &entries {
        let env = if e.required_env.is_empty() {
            String::new()
        } else {
            format!(" · {}", e.required_env.join(", "))
        };
        out.push_str(&format!("  {:<12} {}{}\n", e.name, e.title, env));
    }
    out.push_str(
        "or paste an MCP server config after `/connect` (secrets are extracted to env refs)",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_has_5_to_10_entries_with_names_and_transports() {
        let entries = builtin();
        assert!((5..=10).contains(&entries.len()), "got {}", entries.len());
        for e in &entries {
            assert!(!e.name.is_empty());
            assert!(!e.title.is_empty());
            // Every entry is a connectable connection shape.
            let conn = e.to_connection();
            assert_eq!(conn.name, e.name);
            assert!(conn.enabled);
        }
    }

    #[test]
    fn lookup_finds_notion_with_required_env() {
        let notion = lookup("notion").expect("notion in catalog");
        assert_eq!(notion.required_env, vec!["NOTION_TOKEN"]);
        assert!(lookup("no-such-server").is_none());
    }

    #[test]
    fn check_env_names_missing_vars() {
        let notion = lookup("notion").unwrap();
        assert!(
            notion
                .check_env(|n| (n == "NOTION_TOKEN").then(String::new))
                .is_ok()
        );
        let missing = notion.check_env(|_| None).unwrap_err();
        assert_eq!(missing, vec!["NOTION_TOKEN"]);

        let slack = lookup("slack").unwrap();
        let missing = slack
            .check_env(|n| (n == "SLACK_BOT_TOKEN").then(String::new))
            .unwrap_err();
        assert_eq!(missing, vec!["SLACK_TEAM_ID"]);
    }

    #[test]
    fn catalog_transport_keeps_env_refs_literal() {
        let notion = lookup("notion").unwrap();
        match &notion.transport {
            TransportSpec::Stdio { env, .. } => {
                assert_eq!(env.get("NOTION_TOKEN").unwrap(), "${ENV:NOTION_TOKEN}");
            }
            other => panic!("notion must be stdio, got {other:?}"),
        }
    }

    #[test]
    fn render_catalog_lists_every_entry() {
        let text = render_catalog();
        for e in builtin() {
            assert!(text.contains(&e.name), "catalog line missing {}", e.name);
        }
    }
}
