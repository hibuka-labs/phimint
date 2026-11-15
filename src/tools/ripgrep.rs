//! Content search tool (Phase 2): thin `phi_agent::Tool` shell over
//! `code_intel::ripgrep::search`.

use async_trait::async_trait;
use code_intel::ripgrep;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

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

#[async_trait]
impl Tool for RipgrepTool {
    fn name(&self) -> &'static str {
        "search_content"
    }

    fn description(&self) -> &'static str {
        "Search file contents across the workspace with a regex. Returns matches as `file:line: text`.\n\n\
        Use this when you know what the text should say but not where it lives — \
        a symbol, a string, an error message, a call site. It is the cheapest \
        way to turn \"somewhere in here\" into a file and a line.\n\n\
        Do NOT use it to find a file by name — `list_files` does that. Do NOT \
        use it to understand a codebase's shape: `repo_map` gives you the \
        structure in one call, and grepping your way across a repository to \
        build one is how a cheap question becomes fifty.\n\n\
        Do NOT use it to read what you found. Matches are single lines, not \
        context: a hit tells you where to look, not what the code does. Go to \
        `read_file` with the file and the line.\n\n\
        And a match is not a verdict. A search that finds nothing is not proof \
        the thing is absent — ignored files, binaries and an over-narrow regex \
        all return empty. If absence matters, say what you searched and how.\n\n\
        Quote or escape regex metacharacters when you mean a literal. Prefer a \
        specific pattern to a broad one: the output is capped, and a pattern \
        that matches everywhere returns the beginning of everywhere."
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
            origin: "phimint".to_string(),
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
            return Ok(vec![Content::text(
                "[Error]: no pattern provided".to_string(),
            )]);
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
            ripgrep::search(&root, &pattern, path.as_deref(), max_matches)
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

    fn ctx() -> ToolContext {
        ToolContext::for_test()
    }

    fn text(out: &[Content]) -> String {
        out.iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn metadata_carries_identity() {
        let tool = RipgrepTool::new(std::env::temp_dir());
        assert_eq!(tool.name(), "search_content");
        assert!(!tool.description().is_empty());
        let md = tool.metadata();
        assert_eq!(md.name, "search_content");
        assert_eq!(md.origin, "phimint");
        // Schema names the required argument.
        assert_eq!(tool.schema()["required"][0], "pattern");
    }

    #[tokio::test]
    async fn empty_pattern_is_an_error_not_a_scan() {
        let tool = RipgrepTool::new(std::env::temp_dir());
        let out = tool.call(&json!({"pattern": "  "}), &ctx()).await.unwrap();
        assert!(text(&out).contains("[Error]"), "{}", text(&out));
    }

    #[tokio::test]
    async fn finds_matches_in_temp_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hay.rs"), "fn needle() {}\nfn other() {}\n").unwrap();
        let tool = RipgrepTool::new(dir.path().to_path_buf());

        let out = tool
            .call(&json!({"pattern": "fn needle", "max_results": 5}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("hay.rs"), "{}", text(&out));
        assert!(text(&out).contains("fn needle"), "{}", text(&out));
    }

    #[tokio::test]
    async fn scoped_path_restricts_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("in")).unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("in/a.rs"), "marked\n").unwrap();
        std::fs::write(dir.path().join("out/b.rs"), "marked\n").unwrap();
        let tool = RipgrepTool::new(dir.path().to_path_buf());

        let out = tool
            .call(&json!({"pattern": "marked", "path": "in"}), &ctx())
            .await
            .unwrap();
        assert!(text(&out).contains("in/a.rs"), "{}", text(&out));
        assert!(!text(&out).contains("out/b.rs"), "{}", text(&out));
    }
}
