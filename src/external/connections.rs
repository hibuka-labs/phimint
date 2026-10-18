//! `connections.json` — the MCP connection surface (**only `/connect` writes
//! it**, machine-safe roundtrip).
//!
//! Entry semantics, see design "connections.json entries": stdio shape
//! (command/args/env) or HTTP shape (url/headers). **`${ENV:VAR}` references
//! are saved as-is** (literal fidelity); resolution happens at connection
//! time ([`ConnectionEntry::server_config`]) — if a variable is unset, the
//! error names it on the spot, not at tool-call time.
//!
//! Secret principle: tokens appear only as `${ENV:VAR}` references. The
//! wizard ([`super::wizard`]) is responsible for extracting pasted literal
//! secrets into references; this file only preserves them faithfully.

use std::collections::BTreeMap;
use std::path::Path;

use phi_agent::{EnvRefError, McpServerConfig, McpTransport, resolve_env_map};
use serde::{Deserialize, Serialize};

/// Connection transport. serde tag `type` (`stdio` / `http`), matching the
/// design's JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportSpec {
    /// Local stdio process. `env` is the MCP server's token port (`${ENV:}`
    /// references or non-secret literals).
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Remote HTTP. `headers` likewise (Authorization goes through `${ENV:}`
    /// references).
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// One connection (= one MCP server, tool identity `mcp.{name}.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEntry {
    /// Connection name = server segment. `[a-z0-9_-]{1,32}`, not a reserved
    /// name (mcp/script/plugin).
    pub name: String,
    /// Whether enabled. `/connect` writes true; turning off does not delete
    /// (fidelity roundtrip).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub transport: TransportSpec,
}

fn default_enabled() -> bool {
    true
}

/// Connection-surface file-level errors (whole-file parse failure = load no
/// entries at all + a decent error message).
#[derive(Debug)]
pub enum ConnectionsError {
    Io(std::io::Error),
    Parse(String),
    BadName { name: String },
    Duplicate { name: String },
}

impl std::fmt::Display for ConnectionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read connections.json: {e}"),
            Self::Parse(e) => write!(
                f,
                "connections.json is not valid — no connections loaded ({e})"
            ),
            Self::BadName { name } => write!(
                f,
                "connection name `{name}` must match [a-z0-9_-]{{1,32}} and not be mcp/script/plugin"
            ),
            Self::Duplicate { name } => write!(f, "connection `{name}` already exists"),
        }
    }
}

impl std::error::Error for ConnectionsError {}

/// Name law: `[a-z0-9_-]{1,32}` + not reserved. The `name` segment of the
/// identity key `mcp.{name}.*`.
pub fn valid_connection_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && !matches!(name, "mcp" | "script" | "plugin")
}

/// Load connections.json. Whole-file parse failure → empty table + error
/// (no half-load). Missing file = empty table (first use, not an error).
pub fn load(path: &Path) -> Result<Vec<ConnectionEntry>, ConnectionsError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ConnectionsError::Io(e)),
    };
    let entries: Vec<ConnectionEntry> =
        serde_json::from_str(&raw).map_err(|e| ConnectionsError::Parse(e.to_string()))?;
    for entry in &entries {
        if !valid_connection_name(&entry.name) {
            return Err(ConnectionsError::BadName {
                name: entry.name.clone(),
            });
        }
    }
    // Duplicates (file hand-edited broken) keep the earlier one in file
    // order — same rule as tools.json.
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for entry in entries {
        if seen.insert(entry.name.clone()) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Write connections.json back (pure JSON, machine-safe roundtrip). Creates
/// the directory if missing.
pub fn save(path: &Path, entries: &[ConnectionEntry]) -> Result<(), ConnectionsError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ConnectionsError::Io)?;
    }
    let text = serde_json::to_string_pretty(entries)
        .map_err(|e| ConnectionsError::Parse(e.to_string()))?;
    std::fs::write(path, text).map_err(ConnectionsError::Io)
}

/// Append one connection (duplicates rejected). Does not persist — the
/// caller decides when to [`save`].
pub fn insert(
    entries: &mut Vec<ConnectionEntry>,
    entry: ConnectionEntry,
) -> Result<(), ConnectionsError> {
    if !valid_connection_name(&entry.name) {
        return Err(ConnectionsError::BadName {
            name: entry.name.clone(),
        });
    }
    if entries.iter().any(|e| e.name == entry.name) {
        return Err(ConnectionsError::Duplicate {
            name: entry.name.clone(),
        });
    }
    entries.push(entry);
    Ok(())
}

/// Take out and delete one connection (the `/connect` counterpart / used to
/// replace on reconnect).
pub fn remove(entries: &mut Vec<ConnectionEntry>, name: &str) -> Option<ConnectionEntry> {
    let idx = entries.iter().position(|e| e.name == name)?;
    Some(entries.remove(idx))
}

impl ConnectionEntry {
    /// `${ENV:}` pre-resolution → the literal [`McpServerConfig`] the
    /// framework accepts.
    ///
    /// Timing = once at connection time (design "`${ENV:VAR}` resolution
    /// spec"). A missing variable is reported with its name and the
    /// connection fails — no explosion at call time.
    pub fn server_config<F>(&self, lookup: F) -> Result<McpServerConfig, EnvRefError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let transport = match &self.transport {
            TransportSpec::Stdio { command, args, env } => McpTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: resolve_env_map(env, &lookup)?,
            },
            TransportSpec::Http { url, headers } => McpTransport::Http {
                url: url.clone(),
                headers: resolve_env_map(headers, &lookup)?,
            },
        };
        Ok(McpServerConfig {
            name: self.name.clone(),
            transport,
            auto_reconnect: true,
        })
    }

    /// Environment-variable reference list (the UI's `[env ready]` /
    /// `[missing X]` criterion).
    pub fn env_refs(&self) -> Vec<String> {
        let mut names = Vec::new();
        let map: &BTreeMap<String, String> = match &self.transport {
            TransportSpec::Stdio { env, .. } => env,
            TransportSpec::Http { headers, .. } => headers,
        };
        for value in map.values() {
            collect_env_names(value, &mut names);
        }
        // args may carry references too (e.g. connection strings).
        if let TransportSpec::Stdio { args, .. } = &self.transport {
            for arg in args {
                collect_env_names(arg, &mut names);
            }
        }
        names.sort();
        names.dedup();
        names
    }
}

/// Scan `${ENV:NAME}` names out of a string (values are not resolved).
pub(crate) fn collect_env_names(input: &str, out: &mut Vec<String>) {
    let mut rest = input;
    while let Some(start) = rest.find("${ENV:") {
        let after = &rest[start + 6..];
        let Some(end) = after.find('}') else {
            break;
        };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ConnectionEntry {
        ConnectionEntry {
            name: name.into(),
            enabled: true,
            transport: TransportSpec::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), "@notionhq/notion-mcp-server".into()],
                env: BTreeMap::from([(
                    "NOTION_TOKEN".to_string(),
                    "${ENV:NOTION_TOKEN}".to_string(),
                )]),
            },
        }
    }

    #[test]
    fn roundtrips_stdio_and_http_shapes() {
        let stdio = entry("notion");
        let http = ConnectionEntry {
            name: "figma".into(),
            enabled: true,
            transport: TransportSpec::Http {
                url: "https://mcp.figma.com/mcp".into(),
                headers: BTreeMap::from([(
                    "Authorization".to_string(),
                    "Bearer ${ENV:FIGMA_TOKEN}".to_string(),
                )]),
            },
        };
        let path = std::env::temp_dir().join(format!("conn-{}.json", std::process::id()));
        save(&path, &[stdio.clone(), http.clone()]).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back, vec![stdio, http]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        let path = std::env::temp_dir().join("phimint-no-such-connections.json");
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn whole_file_parse_failure_loads_nothing() {
        let path = std::env::temp_dir().join(format!("conn-bad-{}.json", std::process::id()));
        std::fs::write(&path, "[{\"name\": \"x\"}]").unwrap(); // missing transport
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConnectionsError::Parse(_)), "{err}");
        assert!(err.to_string().contains("no connections loaded"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn name_law_and_duplicates_fail_closed() {
        assert!(valid_connection_name("notion-2"));
        assert!(!valid_connection_name("Notion")); // uppercase
        assert!(!valid_connection_name("mcp")); // reserved name
        assert!(!valid_connection_name(&"a".repeat(33)));
        let mut entries = vec![entry("notion")];
        assert!(matches!(
            insert(&mut entries, entry("notion")),
            Err(ConnectionsError::Duplicate { .. })
        ));
        assert!(matches!(
            insert(&mut entries, entry("Bad Name")),
            Err(ConnectionsError::BadName { .. })
        ));
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn server_config_resolves_env_refs_at_connect_time() {
        let mut e = entry("notion");
        let cfg = e
            .server_config(|n| (n == "NOTION_TOKEN").then(|| "tok-1".to_string()))
            .unwrap();
        assert_eq!(cfg.name, "notion");
        match cfg.transport {
            McpTransport::Stdio { env, .. } => {
                assert_eq!(env.get("NOTION_TOKEN").unwrap(), "tok-1");
            }
            other => panic!("expected stdio, got {other:?}"),
        }

        // Missing variable → error naming the variable (connection fails,
        // never deferred to call time).
        let err = e.server_config(|_| None).unwrap_err();
        assert!(err.to_string().contains("NOTION_TOKEN"), "{err}");

        // Headers on the HTTP shape follow the same spec.
        e.transport = TransportSpec::Http {
            url: "https://mcp.figma.com/mcp".into(),
            headers: BTreeMap::from([(
                "Authorization".to_string(),
                "Bearer ${ENV:FIGMA_TOKEN}".to_string(),
            )]),
        };
        let cfg = e.server_config(|_| Some("t".into())).unwrap();
        match cfg.transport {
            McpTransport::Http { headers, .. } => {
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer t");
            }
            other => panic!("expected http, got {other:?}"),
        }
    }

    #[test]
    fn env_refs_lists_only_referenced_names() {
        let mut e = entry("notion");
        assert_eq!(e.env_refs(), vec!["NOTION_TOKEN".to_string()]);
        if let TransportSpec::Stdio { args, .. } = &mut e.transport {
            args.push("postgres://x?password=${ENV:PG_PASS}".into());
        }
        assert_eq!(
            e.env_refs(),
            vec!["NOTION_TOKEN".to_string(), "PG_PASS".to_string()]
        );
    }
}
