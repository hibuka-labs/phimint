//! Content search tool (Phase 2): wrap ripgrep for symbol/string lookup.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use super::{apply_common_excludes, validate_workspace_path};

/// Hard cap on the formatted result so a broad search can't flood the LLM.
/// Deliberately smaller than the framework's `max_tool_output_chars` (which
/// phiforge raises to 16_000 in `agent::build`) — search hits should stay terse.
const MAX_RESULT_CHARS: usize = 4000;
/// Matches-per-file cap used when the caller doesn't specify one.
const DEFAULT_MAX_MATCHES: usize = 20;

/// Search file contents in the workspace with ripgrep.
pub struct RipgrepTool {
    workspace_root: std::path::PathBuf,
}

impl RipgrepTool {
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        Self { workspace_root }
    }
}

/// Core search logic (sync, testable without the Tool trait).
///
/// Runs `rg --json` scoped to `workspace_root` and formats matches as
/// `file:line: text`. `path` is an optional workspace-relative file/dir.
pub fn search(
    workspace_root: &Path,
    pattern: &str,
    path: Option<&str>,
    max_matches: usize,
) -> Result<String, String> {
    if pattern.is_empty() {
        return Err("empty pattern".to_string());
    }

    let scope = match path {
        Some(p) if !p.trim().is_empty() => validate_workspace_path(workspace_root, p)?,
        _ => ".".to_string(),
    };

    let mut cmd = std::process::Command::new("rg");
    cmd.arg("--json")
        .arg("--max-count")
        .arg(max_matches.to_string())
        .arg("--max-filesize")
        .arg("1M");
    apply_common_excludes(&mut cmd);
    cmd.arg("--")
        .arg(pattern)
        .arg(&scope)
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().map_err(|e| format!("failed to run rg: {e}"))?;

    // Exit codes: 0 = matches, 1 = no matches, 2 = error (bad pattern, etc.).
    if output.status.code() == Some(2) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("rg error: {}", stderr.trim()));
    }

    let mut out = String::new();
    let mut shown = 0usize;
    for raw in output.stdout.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if obj.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let data = &obj["data"];
        let file = data["path"]["text"].as_str().unwrap_or("");
        let line_no = data["line_number"].as_u64().unwrap_or(0);
        let text = data["lines"]["text"].as_str().unwrap_or("").trim_end();

        let entry = format!("{file}:{line_no}: {text}\n");
        if out.len() + entry.len() > MAX_RESULT_CHARS {
            out.push_str("...(truncated)\n");
            break;
        }
        out.push_str(&entry);
        shown += 1;
    }

    if shown == 0 {
        return Ok(format!("No matches for '{pattern}' in '{scope}'."));
    }

    Ok(format!("{shown} match(es) for '{pattern}':\n{out}"))
}

#[async_trait]
impl Tool for RipgrepTool {
    fn name(&self) -> &'static str {
        "search_content"
    }

    fn description(&self) -> &'static str {
        "Search file contents in the workspace with ripgrep (regex). Returns matching lines as `file:line: text`. Use this to locate symbols, function definitions, or strings before reading the relevant file."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for, e.g. `fn get_user`, `pub struct`, `todo!`."
                },
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative file or directory to restrict the search to. Defaults to the whole workspace."
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max matches per file. Default 20."
                }
            },
            "required": ["pattern"]
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Search file contents with ripgrep (regex), workspace-scoped.".to_string(),
            origin: "phiforge".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if pattern.is_empty() {
            return Ok(vec![Content::text("[Error]: no pattern provided".to_string())]);
        }

        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| s.to_string());
        let max_matches = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_MATCHES as u64) as usize;

        tracing::info!(pattern = %pattern, path = ?path, "search_content");

        // `search` does blocking subprocess I/O; keep it off the async runtime.
        let root = self.workspace_root.clone();
        let text = tokio::task::spawn_blocking(move || {
            search(&root, &pattern, path.as_deref(), max_matches)
        })
        .await;

        let text = match text {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => format!("[Error]: {e}"),
            Err(e) => format!("[Error]: search task failed: {e}"),
        };
        Ok(vec![Content::text(text)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn search_finds_matching_lines() {
        let r = search(&crate_root(), "struct RipgrepTool", None, 20).unwrap();
        assert!(r.contains("src/tools/ripgrep.rs"), "{r}");
        assert!(r.contains("struct RipgrepTool"), "{r}");
    }

    #[test]
    fn search_scopes_to_file() {
        let r = search(&crate_root(), "fn search", Some("src/tools/ripgrep.rs"), 20).unwrap();
        assert!(r.contains("fn search"), "{r}");
    }
}
