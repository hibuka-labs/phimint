//! Repository map tool (Phase 2): tree-sitter Rust symbol index.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use super::{apply_common_excludes, validate_workspace_path};

const MAX_FILES: usize = 200;
const MAX_SYMBOLS_PER_FILE: usize = 100;
const MAX_OUTPUT_CHARS: usize = 6000;

/// Produce a structural map of the Rust codebase.
pub struct RepoMapTool {
    workspace_root: std::path::PathBuf,
}

impl RepoMapTool {
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        Self { workspace_root }
    }
}

/// List Rust source files (workspace-relative, respects `.gitignore`) via `rg --files`.
fn list_rust_files(root: &Path, scope: &str) -> Result<Vec<String>, String> {
    let mut cmd = std::process::Command::new("rg");
    cmd.arg("--files").arg("-g").arg("*.rs");
    apply_common_excludes(&mut cmd);
    cmd.arg(scope)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run `rg --files`: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    files.sort();
    Ok(files)
}

/// Byte-slice text of a node's span, or "" if not valid UTF-8 / absent.
fn node_text<'a>(node: tree_sitter::Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// The identifier/type name of an item (via the `name` or `type` field).
fn item_name(node: tree_sitter::Node, source: &[u8]) -> String {
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))
        .map(|c| node_text(c, source).to_string())
        .unwrap_or_default()
}

/// Truncate a string to `max` chars, appending `...` if it overflowed.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let t: String = s.chars().take(max).collect();
        format!("{t}...")
    } else {
        s.to_string()
    }
}

/// Best-effort one-line signature for a top-level item. Signatures are
/// intentionally rough — exact details are left to `read_file`.
fn item_signature(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let sig = match node.kind() {
        "function_item" | "function_signature_item" => {
            let params = node
                .child_by_field_name("parameters")
                .map(|c| node_text(c, source))
                .unwrap_or("");
            let ret = node
                .child_by_field_name("return_type")
                .map(|c| node_text(c, source))
                .unwrap_or("");
            format!("fn {}{} {}", item_name(node, source), params, ret)
                .trim()
                .to_string()
        }
        "struct_item" => format!("struct {}", item_name(node, source)),
        "enum_item" => format!("enum {}", item_name(node, source)),
        "trait_item" => format!("trait {}", item_name(node, source)),
        "impl_item" => {
            let ty = node
                .child_by_field_name("type")
                .map(|c| node_text(c, source))
                .unwrap_or("");
            match node.child_by_field_name("trait") {
                Some(t) => format!("impl {} for {}", node_text(t, source), ty),
                None => format!("impl {ty}"),
            }
        }
        "mod_item" => format!("mod {}", item_name(node, source)),
        "type_item" => format!("type {}", item_name(node, source)),
        "const_item" => format!("const {}", item_name(node, source)),
        "static_item" => format!("static {}", item_name(node, source)),
        "use_declaration" => {
            let first = node_text(node, source).lines().next().unwrap_or("");
            truncate(first.trim(), 60)
        }
        _ => return None,
    };
    Some(sig)
}

fn is_item_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "trait_item"
            | "impl_item"
            | "mod_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "use_declaration"
    )
}

/// Core: build the repo map as a compact string (sync, testable without the Tool trait).
pub fn build_repo_map(root: &Path, scope: Option<&str>) -> Result<String, String> {
    let scope = match scope {
        Some(s) if !s.trim().is_empty() => validate_workspace_path(root, s)?,
        _ => ".".to_string(),
    };

    let files = list_rust_files(root, &scope)?;
    if files.is_empty() {
        return Ok("(no .rs files found in workspace)".to_string());
    }

    let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| format!("failed to load Rust grammar: {e}"))?;

    let mut out = String::new();
    let mut file_count = 0usize;

    for file in files.iter().take(MAX_FILES) {
        let source = match std::fs::read(root.join(file)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tree = match parser.parse(&source, None) {
            Some(t) => t,
            None => continue,
        };

        let root_node = tree.root_node();
        let mut symbols: Vec<String> = Vec::new();
        for i in 0..root_node.named_child_count() {
            if symbols.len() >= MAX_SYMBOLS_PER_FILE {
                break;
            }
            let Some(child) = root_node.named_child(i) else {
                continue;
            };
            if !is_item_kind(child.kind()) {
                continue;
            }
            if let Some(sig) = item_signature(child, &source) {
                let line = child.start_position().row + 1;
                symbols.push(format!("  {sig}  (L{line})"));
            }
        }

        if !symbols.is_empty() {
            if file_count > 0 {
                out.push('\n');
            }
            out.push_str(&format!("{file}\n"));
            out.push_str(&symbols.join("\n"));
            out.push('\n');
            file_count += 1;
        }

        if out.len() >= MAX_OUTPUT_CHARS {
            break;
        }
    }

    if out.is_empty() {
        return Ok("(no Rust symbols found)".to_string());
    }

    let mut result = format!("Repository map ({file_count} files shown):\n\n{out}");
    if result.len() > MAX_OUTPUT_CHARS {
        result.truncate(MAX_OUTPUT_CHARS);
        result.push_str("\n...(truncated)\n");
    }
    Ok(result)
}

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &'static str {
        "repo_map"
    }

    fn description(&self) -> &'static str {
        "Produce a structural map of the Rust codebase: files and their top-level symbols (functions, structs, enums, traits, impls, modules). Call this FIRST to understand the codebase layout before reading specific files."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative directory to map. Defaults to the whole workspace."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Produce a structural map of the Rust codebase.".to_string(),
            origin: "phiforge".to_string(),
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

        let root = self.workspace_root.clone();
        let text = tokio::task::spawn_blocking(move || build_repo_map(&root, path.as_deref())).await;

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

    fn crate_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn build_repo_map_extracts_known_symbols() {
        let map = build_repo_map(&crate_root(), None).unwrap();
        assert!(map.contains("src/tools/ripgrep.rs"), "should list ripgrep.rs:\n{map}");
        assert!(map.contains("struct RipgrepTool"), "should extract struct:\n{map}");
        assert!(map.contains("fn search"), "should extract fn signature:\n{map}");
        assert!(!map.contains("target/"), "must not map build output:\n{map}");
    }

    #[test]
    fn build_repo_map_scopes_to_subdir() {
        let map = build_repo_map(&crate_root(), Some("src/tools")).unwrap();
        assert!(map.contains("ripgrep.rs"), "subdir map should list ripgrep.rs:\n{map}");
    }
}
