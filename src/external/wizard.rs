//! Paste wizard: MCP server config → connection entry + **secret
//! extraction**.
//!
//! Design wording (product surface): "the wizard extracts secrets → prints
//! placeholder export templates, never echoes". Criteria:
//!
//! - the paste accepts the Claude-Desktop shape (`mcpServers` wrapper / bare
//!   server definition / our own entry shape), transcribed into a
//!   [`ConnectionEntry`];
//! - **literal secrets never hit disk**: matched env/headers values are
//!   extracted into `${ENV:VAR}` references and the values are discarded on
//!   the spot; what is printed is a **placeholder export**
//!   (`export VAR="<paste …>"`) for the user to paste into — `export_lines`
//!   never carry the real value (no echo);
//! - non-secret literals (`LOG_LEVEL=debug` and friends) are kept as-is;
//! - with no name, take the command's first segment / package name's last
//!   segment, normalize it, and run it through the name law.

use std::collections::BTreeMap;

use super::connections::{ConnectionEntry, TransportSpec, valid_connection_name};

/// One extracted secret (metadata only; the value is not kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSecret {
    /// Placeholder environment variable name (for the `export` printout).
    pub var: String,
    /// Original key name (`Authorization` / `NOTION_TOKEN`…), for reporting.
    pub key: String,
    /// Whether the value carried a `Bearer ` prefix (the template keeps the
    /// prefix; only the token part is extracted).
    pub bearer_prefix: bool,
}

/// Wizard output.
#[derive(Debug)]
pub struct WizardOutcome {
    pub entry: ConnectionEntry,
    pub extracted: Vec<ExtractedSecret>,
    /// Placeholder export lines (no real values; safe to hand to the user
    /// to copy).
    pub export_lines: Vec<String>,
    pub warnings: Vec<String>,
}

/// Wizard errors (the paste is not a config at all).
#[derive(Debug)]
pub enum WizardError {
    NotJson(String),
    NoServer,
    BadName { name: String },
}

impl std::fmt::Display for WizardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotJson(e) => write!(
                f,
                "that does not look like an MCP server config (not JSON: {e})"
            ),
            Self::NoServer => write!(
                f,
                "no server definition found — paste a Claude Desktop `mcpServers` config or a server object"
            ),
            Self::BadName { name } => write!(
                f,
                "cannot derive a valid connection name from `{name}` (want [a-z0-9_-]{{1,32}})"
            ),
        }
    }
}

impl std::error::Error for WizardError {}

/// Patterns for keys that look secret (conservative: over-extract rather
/// than let one leak to disk).
fn key_looks_secret(key: &str) -> bool {
    const PATTERNS: [&str; 12] = [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "PAT",
        "AUTH",
        "DSN",
        "URL",
        "CONN",
        "CERT",
    ];
    let upper = key.to_ascii_uppercase();
    PATTERNS.iter().any(|p| upper.contains(p))
}

/// Value looks secret: Bearer shape / known token prefix / high-entropy
/// long string.
fn value_looks_secret(value: &str) -> bool {
    if value.starts_with("Bearer ") && value.len() > "Bearer ".len() + 8 {
        return true;
    }
    const PREFIXES: [&str; 8] = [
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "sk-",
        "xoxb-",
        "xoxp-",
        "ya29.",
    ];
    if PREFIXES.iter().any(|p| value.starts_with(p)) {
        return true;
    }
    // High-entropy long string: ≥20 chars, no whitespace, mixed alphanumerics.
    value.len() >= 20
        && !value.contains(char::is_whitespace)
        && value.chars().any(|c| c.is_ascii_alphabetic())
        && value.chars().any(|c| c.is_ascii_digit())
}

/// Placeholder variable name: if the key itself is ENV-shaped, use it
/// directly; generic names (Authorization and friends) hang off the server
/// name (`figma` + `Authorization` → `FIGMA_TOKEN`).
fn derive_var_name(server: &str, key: &str, bearer: bool) -> String {
    let is_env_shaped = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    if is_env_shaped {
        return key.to_string();
    }
    let server = sanitize_env_name(server);
    let generic = matches!(
        key.to_ascii_uppercase().as_str(),
        "AUTHORIZATION" | "AUTH" | "TOKEN" | "KEY" | "SECRET" | "PASSWORD" | "API-KEY" | "API_KEY"
    );
    if generic || bearer {
        format!("{server}_TOKEN")
    } else {
        format!("{server}_{}", sanitize_env_name(key))
    }
}

fn sanitize_env_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() { "VALUE".into() } else { out }
}

/// Extract values from one map (env / headers): secrets → `${ENV:VAR}`
/// references + records; non-secrets kept as-is. Returns (new map,
/// extraction records).
fn extract_map(
    server: &str,
    map: &BTreeMap<String, String>,
    used_vars: &mut std::collections::BTreeSet<String>,
    extracted: &mut Vec<ExtractedSecret>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        // Already a reference → preserve as-is.
        if value.contains("${ENV:") {
            out.insert(key.clone(), value.clone());
            continue;
        }
        let secret = key_looks_secret(key) || value_looks_secret(value);
        if !secret {
            out.insert(key.clone(), value.clone());
            continue;
        }
        let (bearer_prefix, token_value) = match value.strip_prefix("Bearer ") {
            Some(rest) => (true, rest),
            None => (false, value.as_str()),
        };
        // Variable-name dedup (same-name references share one export).
        let mut var = derive_var_name(server, key, bearer_prefix || value.starts_with("Bearer "));
        let mut n = 2;
        while used_vars.contains(&var) {
            var = format!("{}_{}", derive_var_name(server, key, bearer_prefix), n);
            n += 1;
        }
        let _ = token_value; // the value is discarded immediately — no echo, no disk, not kept in the outcome
        used_vars.insert(var.clone());
        let templated = if bearer_prefix {
            format!("Bearer ${{ENV:{var}}}")
        } else {
            format!("${{ENV:{var}}}")
        };
        out.insert(key.clone(), templated);
        extracted.push(ExtractedSecret {
            var,
            key: key.clone(),
            bearer_prefix,
        });
    }
    out
}

/// Parse a server definition out of the pasted text (Claude-Desktop shape →
/// our entry).
pub fn parse_paste(text: &str) -> Result<WizardOutcome, WizardError> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| WizardError::NotJson(e.to_string()))?;
    let (name_hint, def) = split_server_def(&value).ok_or(WizardError::NoServer)?;
    let def = match def {
        ServerDef::Plain(def) => def,
        // Our own entry shape: {"name": "x", "transport": {...}}.
        ServerDef::Entry(entry) => return finish_entry(entry),
    };

    // def is a server definition object (command/args/env or url/headers).
    let obj = match def.as_object() {
        Some(obj) if !obj.is_empty() => obj.clone(),
        _ => return Err(WizardError::NoServer),
    };

    let mut warnings = Vec::new();
    let mut extracted = Vec::new();
    let mut used_vars = std::collections::BTreeSet::new();

    let transport = if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
        let headers: BTreeMap<String, String> = obj
            .get("headers")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let headers = extract_map(
            server_name_guess(name_hint.as_deref(), &obj).as_str(),
            &headers,
            &mut used_vars,
            &mut extracted,
        );
        TransportSpec::Http {
            url: url.to_string(),
            headers,
        }
    } else if let Some(command) = obj.get("command").and_then(|v| v.as_str()) {
        let args: Vec<String> = obj
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let env: BTreeMap<String, String> = obj
            .get("env")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let env = extract_map(
            server_name_guess(name_hint.as_deref(), &obj).as_str(),
            &env,
            &mut used_vars,
            &mut extracted,
        );
        TransportSpec::Stdio {
            command: command.to_string(),
            args,
            env,
        }
    } else {
        return Err(WizardError::NoServer);
    };

    let name = derive_name(name_hint.as_deref(), &transport)?;
    let entry = ConnectionEntry {
        name,
        enabled: true,
        transport,
    };
    if extracted.is_empty() {
        warnings.push("no secrets found in the pasted config — values kept as-is".into());
    }
    Ok(outcome(entry, extracted, warnings))
}

enum ServerDef {
    Plain(serde_json::Value),
    Entry(ConnectionEntry),
}

/// Split the pasted shape: `mcpServers` wrapper / bare map / single object /
/// our entry shape.
fn split_server_def(value: &serde_json::Value) -> Option<(Option<String>, ServerDef)> {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(servers) = map.get("mcpServers").and_then(|v| v.as_object()) {
                let (name, def) = servers.iter().next()?;
                return Some((Some(name.clone()), ServerDef::Plain(def.clone())));
            }
            // Our entry shape (has name + transport).
            if map.contains_key("name") && map.contains_key("transport") {
                let entry: ConnectionEntry = serde_json::from_value(value.clone()).ok()?;
                return Some((Some(entry.name.clone()), ServerDef::Entry(entry)));
            }
            // Bare map: one key → server definition.
            if map.len() == 1 {
                let (name, def) = map.iter().next()?;
                if def.is_object() {
                    return Some((Some(name.clone()), ServerDef::Plain(def.clone())));
                }
            }
            // Single-object server definition (command / url).
            if map.contains_key("command") || map.contains_key("url") {
                return Some((None, ServerDef::Plain(value.clone())));
            }
            None
        }
        _ => None,
    }
}

fn finish_entry(entry: ConnectionEntry) -> Result<WizardOutcome, WizardError> {
    let name = entry.name.clone();
    if !valid_connection_name(&name) {
        return Err(WizardError::BadName { name });
    }
    // The entry shape also goes through secret extraction (the user may
    // have pasted an old entry straight with literals in it).
    let mut extracted = Vec::new();
    let mut used_vars = std::collections::BTreeSet::new();
    let mut warnings = Vec::new();
    let transport = match &entry.transport {
        TransportSpec::Stdio { command, args, env } => {
            let env = extract_map(&name, env, &mut used_vars, &mut extracted);
            TransportSpec::Stdio {
                command: command.clone(),
                args: args.clone(),
                env,
            }
        }
        TransportSpec::Http { url, headers } => {
            let headers = extract_map(&name, headers, &mut used_vars, &mut extracted);
            TransportSpec::Http {
                url: url.clone(),
                headers,
            }
        }
    };
    if extracted.is_empty() {
        warnings.push("no secrets found in the pasted config — values kept as-is".into());
    }
    Ok(outcome(
        ConnectionEntry {
            name: entry.name,
            enabled: entry.enabled,
            transport,
        },
        extracted,
        warnings,
    ))
}

fn server_name_guess(
    hint: Option<&str>,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> String {
    if let Some(hint) = hint {
        return hint.to_string();
    }
    if let Some(url) = obj.get("url").and_then(|v| v.as_str()) {
        return derive_name_from_url(url);
    }
    command_stem(obj).unwrap_or_else(|| "mcp".into())
}

fn derive_name_from_url(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', ':']).next())
        .map(|host| {
            host.split('.')
                .next()
                .unwrap_or(host)
                .chars()
                .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_')
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Command's first segment / package name's last segment
/// (`npx -y @notionhq/notion-mcp-server` → `notion-mcp-server`).
fn command_stem(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let command = obj.get("command").and_then(|v| v.as_str())?;
    let stem = std::path::Path::new(command)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| command.to_lowercase());
    const LAUNCHERS: [&str; 7] = ["npx", "node", "bun", "deno", "python", "python3", "uvx"];
    if LAUNCHERS.contains(&stem.as_str())
        && let Some(args) = obj.get("args").and_then(|v| v.as_array())
    {
        for arg in args {
            let arg = arg.as_str().unwrap_or("");
            if arg.starts_with('-') {
                continue;
            }
            let last = arg
                .rsplit(['/', '@'])
                .find(|s| !s.is_empty())
                .unwrap_or(arg);
            return Some(last.to_string());
        }
    }
    Some(stem)
}

fn derive_name(hint: Option<&str>, transport: &TransportSpec) -> Result<String, WizardError> {
    let raw = match hint {
        Some(hint) if !hint.is_empty() => hint.to_string(),
        _ => match transport {
            TransportSpec::Stdio { command, .. } => {
                let obj = serde_json::json!({"command": command});
                let mut map = serde_json::Map::new();
                map.insert("command".into(), command.clone().into());
                if let TransportSpec::Stdio { args, .. } = transport {
                    map.insert("args".into(), args.clone().into());
                }
                command_stem(&map).unwrap_or_else(|| {
                    obj.get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("mcp")
                        .to_string()
                })
            }
            TransportSpec::Http { url, .. } => derive_name_from_url(url),
        },
    };
    let name: String = raw
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .chars()
        .take(32)
        .collect();
    let name = name.trim_matches('-').to_string();
    if valid_connection_name(&name) {
        Ok(name)
    } else {
        Err(WizardError::BadName { name: raw })
    }
}

fn outcome(
    entry: ConnectionEntry,
    extracted: Vec<ExtractedSecret>,
    warnings: Vec<String>,
) -> WizardOutcome {
    let export_lines = extracted
        .iter()
        .map(|s| {
            let what = if s.bearer_prefix {
                "paste the token (without the Bearer prefix)"
            } else {
                "paste the value"
            };
            format!("export {}=\"{}\"", s.var, what)
        })
        .collect();
    WizardOutcome {
        entry,
        extracted,
        export_lines,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(entry: &ConnectionEntry) -> &BTreeMap<String, String> {
        match &entry.transport {
            TransportSpec::Stdio { env, .. } => env,
            TransportSpec::Http { headers, .. } => headers,
        }
    }

    #[test]
    fn claude_desktop_paste_extracts_token_never_keeps_it() {
        let paste = r#"{
            "mcpServers": {
                "notion": {
                    "command": "npx",
                    "args": ["-y", "@notionhq/notion-mcp-server"],
                    "env": { "NOTION_TOKEN": "ntn_secret_abc123def456ghi789jkl" }
                }
            }
        }"#;
        let out = parse_paste(paste).unwrap();
        assert_eq!(out.entry.name, "notion");
        let env = env_of(&out.entry);
        assert_eq!(env.get("NOTION_TOKEN").unwrap(), "${ENV:NOTION_TOKEN}");
        // No literal may linger (it must not appear on any serialization
        // surface).
        let dumped = serde_json::to_string(&out.entry).unwrap();
        assert!(
            !dumped.contains("ntn_secret"),
            "secret leaked into entry: {dumped}"
        );
        assert_eq!(out.extracted.len(), 1);
        assert_eq!(out.extracted[0].var, "NOTION_TOKEN");
        // Placeholder export: has the variable name, no real value.
        assert_eq!(
            out.export_lines,
            vec![r#"export NOTION_TOKEN="paste the value""#.to_string()]
        );
        for line in &out.export_lines {
            assert!(!line.contains("ntn_secret"));
        }
    }

    #[test]
    fn bearer_header_keeps_prefix_and_exports_token_only() {
        let paste = r#"{
            "figma": {
                "url": "https://mcp.figma.com/mcp",
                "headers": { "Authorization": "Bearer ghp_abc123def456ghi789" }
            }
        }"#;
        let out = parse_paste(paste).unwrap();
        assert_eq!(out.entry.name, "figma");
        let headers = env_of(&out.entry);
        assert_eq!(
            headers.get("Authorization").unwrap(),
            "Bearer ${ENV:FIGMA_TOKEN}"
        );
        assert_eq!(out.extracted[0].var, "FIGMA_TOKEN");
        assert!(out.extracted[0].bearer_prefix);
        assert!(out.export_lines[0].contains("without the Bearer prefix"));
        let dumped = serde_json::to_string(&out.entry).unwrap();
        assert!(!dumped.contains("ghp_"), "token leaked: {dumped}");
    }

    #[test]
    fn non_secret_literals_pass_through() {
        let paste = r#"{
            "logs": {
                "command": "node",
                "args": ["/opt/logs/server.js"],
                "env": { "LOG_LEVEL": "debug", "PORT": "8080" }
            }
        }"#;
        let out = parse_paste(paste).unwrap();
        let env = env_of(&out.entry);
        assert_eq!(env.get("LOG_LEVEL").unwrap(), "debug");
        assert_eq!(env.get("PORT").unwrap(), "8080");
        assert!(out.extracted.is_empty());
    }

    #[test]
    fn existing_env_refs_are_preserved_not_re_extracted() {
        let paste = r#"{
            "name": "notion",
            "transport": {
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@notionhq/notion-mcp-server"],
                "env": { "NOTION_TOKEN": "${ENV:NOTION_TOKEN}" }
            }
        }"#;
        let out = parse_paste(paste).unwrap();
        assert_eq!(
            env_of(&out.entry).get("NOTION_TOKEN").unwrap(),
            "${ENV:NOTION_TOKEN}"
        );
        assert!(out.extracted.is_empty());
    }

    #[test]
    fn name_derivation_uses_package_stem_for_launchers() {
        let paste = r#"{
            "command": "npx",
            "args": ["-y", "@notionhq/notion-mcp-server"],
            "env": {}
        }"#;
        let out = parse_paste(paste).unwrap();
        assert_eq!(out.entry.name, "notion-mcp-server");
    }

    #[test]
    fn garbage_paste_reports_how_it_failed() {
        let err = parse_paste("not json").unwrap_err();
        assert!(err.to_string().contains("not JSON"), "{err}");
        let err = parse_paste(r#"{"hello": 1}"#).unwrap_err();
        assert!(matches!(err, WizardError::NoServer), "{err}");
    }
}
