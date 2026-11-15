//! Repository map tool (Phase 2): thin `phi_agent::Tool` shell over
//! `code_intel::repomap` (tree-sitter symbol map + directory skeleton).

use async_trait::async_trait;
use code_intel::repomap::{build_dir_tree, build_repo_map};
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

/// Produce a structural map of the codebase.
pub struct RepoMapTool {
    workspace_root: std::path::PathBuf,
}

impl RepoMapTool {
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        Self { workspace_root }
    }
}

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &'static str {
        "repo_map"
    }

    fn description(&self) -> &'static str {
        "A structural map of the codebase. No `path` gives a directory skeleton \
        (module → package tree with file counts); a workspace-relative `path` \
        gives per-file symbols (classes, methods, fields, …) with line ranges \
        and each file's line count.\n\n\
        Use this before you go file by file. When the task needs you to find \
        your way around a repository — where the entry point is, which modules \
        touch a thing, what a directory holds — one call here replaces a dozen \
        reads, and it tells you which files are worth opening at all.\n\n\
        Do NOT use it to find specific text — `search_content` answers \"where \
        does this string appear\" in one call. Do NOT use it to read: a symbol \
        list tells you a function exists and where it starts, never what it \
        does. Once you know which file matters, `read_file` that file.\n\n\
        A map is not the territory. Symbols are names and line ranges, not \
        behaviour; a shape you inferred from the map is a hypothesis to check in \
        the file, not something to report as fact.\n\n\
        Start with the whole-workspace skeleton when you do not yet know where \
        to look; narrow to a `path` when you know the area and want its symbols."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative directory. Omit for the whole-workspace directory skeleton; pass a directory for per-file symbols."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Produce a structural map of the codebase.".to_string(),
            origin: "phimint".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        tracing::info!(path = ?path, "repo_map");

        // No `path` → whole-workspace directory skeleton; a scoped `path` → the
        // per-file symbol map (too large to fit globally).
        let scoped = path
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let root = self.workspace_root.clone();
        let text = tokio::task::spawn_blocking(move || {
            if scoped {
                build_repo_map(&root, path.as_deref())
            } else {
                build_dir_tree(&root, path.as_deref())
            }
        })
        .await;

        let text = match text {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => format!("[Error]: {e}"),
            Err(e) => format!("[Error]: repo map task failed: {e}"),
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

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub struct Widget;\nimpl Widget { fn render(&self) {} }\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn metadata_carries_identity() {
        let tool = RepoMapTool::new(std::env::temp_dir());
        assert_eq!(tool.name(), "repo_map");
        let md = tool.metadata();
        assert_eq!(md.name, "repo_map");
        assert_eq!(md.origin, "phimint");
        assert!(tool.schema()["properties"].get("path").is_some());
    }

    #[tokio::test]
    async fn bare_call_returns_directory_skeleton() {
        let dir = workspace();
        let tool = RepoMapTool::new(dir.path().to_path_buf());

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        // Bare call returns the layout header + per-module file counts, with the
        // package path stripped as a common prefix — never per-file symbols.
        let t = text(&out);
        assert!(t.contains("Repository layout"), "{t}");
        assert!(t.contains("(1 file)"), "{t}");
        assert!(
            !t.contains("Widget"),
            "bare call must not include per-file symbols: {t}"
        );
    }

    #[tokio::test]
    async fn scoped_path_returns_per_file_symbols() {
        let dir = workspace();
        let tool = RepoMapTool::new(dir.path().to_path_buf());

        let out = tool.call(&json!({"path": "src"}), &ctx()).await.unwrap();
        assert!(text(&out).contains("lib.rs"), "{}", text(&out));
        assert!(text(&out).contains("Widget"), "{}", text(&out));
    }
}
