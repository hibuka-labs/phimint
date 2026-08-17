//! Repository map tool (Phase 2): tree-sitter symbol index across languages.
//!
//! A terse structural map of the workspace: each source file, followed by the
//! symbols extracted from it (classes, functions, methods, fields, …) with their
//! line numbers. Two extractors:
//!
//! - **Rust** keeps a hand-written `item_signature` for precision (full `fn
//!   name(params) -> ret` / `impl X for Y` signatures).
//! - **Every other registered language** (Java, TypeScript/JS/JSX, C, C++) runs
//!   the generic extractor: a `name → declarator → type` field chain, node-kind
//!   substring classification, and a bounded one-level descent into class-like
//!   bodies (methods/fields). Rough by design — exact details are left to
//!   `read_file`.

use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use super::{apply_common_excludes, validate_workspace_path};
use crate::lang::language_for_path;

const MAX_FILES: usize = 200;
const MAX_SYMBOLS_PER_FILE: usize = 100;
const MAX_OUTPUT_CHARS: usize = 6000;
/// Budget for the whole-workspace directory skeleton (larger than the per-file
/// symbol budget: a package tree is ~30 chars/line vs ~40 chars/symbol, and it
/// must list *every* module header without the alphabetical-slice bias that
/// truncation introduces).
const MAX_DIR_TREE_CHARS: usize = 9000;
/// How many levels of class-like nesting to descend for members. The top level
/// is depth 0; class bodies are depth 1 (their members), and we stop there.
const MAX_DEPTH: u8 = 1;

/// Produce a structural map of the codebase.
pub struct RepoMapTool {
    workspace_root: std::path::PathBuf,
}

impl RepoMapTool {
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        Self { workspace_root }
    }
}

/// List source files of any registered language (workspace-relative, respects
/// `.gitignore`) via `rg --files`, filtered to known extensions in Rust.
fn list_code_files(root: &Path, scope: &str) -> Result<Vec<String>, String> {
    let mut cmd = std::process::Command::new("rg");
    cmd.arg("--files");
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
        .filter(|l| language_for_path(l).is_some())
        .collect();
    files.sort();
    Ok(files)
}

/// The tree-sitter grammar for a source file, by extension.
fn grammar_for(path: &str) -> Option<tree_sitter::Language> {
    let ext = Path::new(path).extension()?.to_str()?;
    Some(match ext {
        "rs" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "js" | "jsx" | "mjs" | "cjs" => tree_sitter_javascript::LANGUAGE.into(),
        "c" | "h" => tree_sitter_c::LANGUAGE.into(),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => tree_sitter_cpp::LANGUAGE.into(),
        _ => return None,
    })
}

/// Byte-slice text of a node's span, or "" if not valid UTF-8 / absent.
fn node_text<'a>(node: tree_sitter::Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
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

// ---------------------------------------------------------------------------
// Rust: hand-written precision extractor.
// ---------------------------------------------------------------------------

/// The identifier/type name of a Rust item (via the `name` or `type` field).
fn item_name(node: tree_sitter::Node, source: &[u8]) -> String {
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))
        .map(|c| node_text(c, source).to_string())
        .unwrap_or_default()
}

/// Best-effort one-line signature for a top-level Rust item. Signatures are
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

/// Extract top-level Rust symbols (no class-body descent — Rust is flat).
fn extract_rust_symbols(root_node: tree_sitter::Node, source: &[u8], symbols: &mut Vec<String>) {
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
        if let Some(sig) = item_signature(child, source) {
            let line = child.start_position().row + 1;
            symbols.push(format!("  {sig}  (L{line})"));
        }
    }
}

// ---------------------------------------------------------------------------
// Generic extractor for every other registered language.
// ---------------------------------------------------------------------------

/// Resolve a declaration's name via the `name → declarator → type` field chain,
/// drilling one level into C++ `function_declarator`/`pointer_declarator`.
fn generic_name(node: tree_sitter::Node, source: &[u8]) -> String {
    for field in ["name", "declarator", "type"] {
        let Some(c) = node.child_by_field_name(field) else {
            continue;
        };
        if let Some(inner) = c
            .child_by_field_name("declarator")
            .or_else(|| c.child_by_field_name("name"))
        {
            let t = node_text(inner, source).trim().to_string();
            if !t.is_empty() {
                return t;
            }
        }
        let t = node_text(c, source).trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    String::new()
}

/// Classify a node kind into a short label, or `None` for noise/irrelevant
/// nodes. Substring matching covers the cross-language naming divergence
/// (`struct_item` vs `struct_specifier` vs `struct_declaration`).
fn kind_label(kind: &str) -> Option<&'static str> {
    // Noise that substring-matches a label below but is not a declaration.
    if kind.contains("expression")
        || kind.contains("statement")
        || kind.contains("arrow")
        || kind.contains("lambda")
        || kind.contains("parameter")
        || kind.contains("argument")
        || kind.contains("variable")
        || kind.contains("lexical")
    {
        return None;
    }
    // Order matters: `constructor` contains `struct`.
    if kind.contains("constructor") {
        return Some("constructor");
    }
    if kind.contains("interface") {
        return Some("interface");
    }
    if kind.contains("enum") {
        return Some("enum");
    }
    if kind.contains("trait") {
        return Some("trait");
    }
    if kind.contains("record") {
        return Some("record");
    }
    if kind.contains("namespace") {
        return Some("namespace");
    }
    if kind.contains("module") {
        return Some("module");
    }
    if kind.contains("struct") {
        return Some("struct");
    }
    if kind.contains("class") {
        return Some("class");
    }
    if kind.contains("method") {
        return Some("method");
    }
    if kind.contains("function") {
        return Some("fn");
    }
    if kind.contains("property") {
        return Some("field");
    }
    if kind.contains("field") {
        return Some("field");
    }
    if kind.contains("type_alias") {
        return Some("type");
    }
    if kind.contains("concept") {
        return Some("concept");
    }
    None
}

/// Kinds we descend *into* to list members, consuming one depth level.
fn is_class_body(kind: &str) -> bool {
    // `constructor` contains `struct` but is a member, not a body.
    if kind.contains("constructor") {
        return false;
    }
    kind.contains("class")
        || kind.contains("struct")
        || kind.contains("interface")
        || kind.contains("namespace")
        || kind.contains("module")
        || kind.contains("record")
        || kind.contains("trait")
}

/// Transparent nodes we descend through *without* consuming a depth level:
/// - wrappers around a single declaration (TS `export_statement`, C++
///   `template_declaration`);
/// - body-list nodes that merely group a class/interface/enum's members
///   (`class_body`, C++ `field_declaration_list`, …). Members live one node
///   deeper than the declaration, so treating the body list as transparent
///   keeps "one level of class nesting" == one depth step.
fn is_transparent_wrapper(kind: &str) -> bool {
    matches!(kind, "export_statement" | "template_declaration")
        || kind.ends_with("_body")
        || matches!(kind, "field_declaration_list" | "declaration_list" | "enumerator_list")
}

/// Recursively collect symbols from a node's children.
fn extract_generic_symbols(
    node: tree_sitter::Node,
    source: &[u8],
    depth: u8,
    symbols: &mut Vec<String>,
) {
    for i in 0..node.named_child_count() {
        if symbols.len() >= MAX_SYMBOLS_PER_FILE {
            return;
        }
        let Some(child) = node.named_child(i) else {
            continue;
        };
        let kind = child.kind();

        // Body-list / wrapper nodes group members or wrap a declaration — and
        // some (`class_body`) also substring-match a label below, so check them
        // first and descend without consuming depth or classifying them.
        if is_transparent_wrapper(kind) {
            extract_generic_symbols(child, source, depth, symbols);
            continue;
        }

        if let Some(label) = kind_label(kind) {
            let name = generic_name(child, source);
            if !name.is_empty() {
                let line = child.start_position().row + 1;
                symbols.push(format!("  {label} {name}  (L{line})"));
            }
            if is_class_body(kind) && depth < MAX_DEPTH {
                extract_generic_symbols(child, source, depth + 1, symbols);
            }
        }
    }
}

/// Core: build the repo map as a compact string (sync, testable without the Tool trait).
pub fn build_repo_map(root: &Path, scope: Option<&str>) -> Result<String, String> {
    let scope = match scope {
        Some(s) if !s.trim().is_empty() => validate_workspace_path(root, s)?,
        _ => ".".to_string(),
    };

    let files = list_code_files(root, &scope)?;
    if files.is_empty() {
        return Ok("(no source files found in workspace)".to_string());
    }

    let mut parser = tree_sitter::Parser::new();
    let mut out = String::new();
    let mut file_count = 0usize;

    for file in files.iter().take(MAX_FILES) {
        let Some(lang) = grammar_for(file) else {
            continue;
        };
        if let Err(e) = parser.set_language(&lang) {
            tracing::warn!(file = %file, error = %e, "repo_map: unsupported grammar");
            continue;
        }
        let source = match std::fs::read(root.join(file)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let tree = match parser.parse(&source, None) {
            Some(t) => t,
            None => continue,
        };

        let mut symbols: Vec<String> = Vec::new();
        if file.ends_with(".rs") {
            extract_rust_symbols(tree.root_node(), &source, &mut symbols);
        } else {
            extract_generic_symbols(tree.root_node(), &source, 0, &mut symbols);
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
        return Ok("(no symbols found)".to_string());
    }

    let mut result = format!("Repository map ({file_count} files shown):\n\n{out}");
    if result.len() > MAX_OUTPUT_CHARS {
        result.truncate(MAX_OUTPUT_CHARS);
        result.push_str("\n...(truncated)\n");
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Directory skeleton: the whole-workspace layout.
// ---------------------------------------------------------------------------

/// A node in the package tree: files directly in this dir plus subdirectories.
struct DirNode {
    count: usize,
    children: std::collections::BTreeMap<String, DirNode>,
}

impl DirNode {
    fn new() -> Self {
        DirNode {
            count: 0,
            children: std::collections::BTreeMap::new(),
        }
    }

    fn insert(&mut self, comps: &[String], count: usize) {
        let mut cur = self;
        for c in comps {
            cur = cur.children.entry(c.clone()).or_insert_with(DirNode::new);
        }
        cur.count += count;
    }

    fn render(&self, out: &mut String, name: &str, indent: usize) {
        let pad = "  ".repeat(indent);
        let count = if self.count > 0 {
            format!(" ({})", self.count)
        } else {
            String::new()
        };
        out.push_str(&format!("{pad}{name}/{count}\n"));
        for (cn, child) in &self.children {
            child.render(out, cn, indent + 1);
        }
    }
}

/// Split a workspace-relative source path into `(module, package-components)` by
/// locating the Maven `src/{main,test}/java` marker. `module` is the path up to
/// (but excluding) `src`; the package is the directory components after `java`,
/// minus the filename. `None` when the marker is absent (non-Maven layout).
fn split_module_package(path: &str) -> Option<(String, Vec<String>)> {
    let comps: Vec<&str> = path.split('/').collect();
    let si = comps.iter().position(|c| *c == "src")?;
    let ji = (si + 1..comps.len()).find(|&i| comps[i] == "java")?;
    if ji + 1 >= comps.len() {
        return None; // no filename component after `java`.
    }
    let module = comps[..si].join("/");
    let pkg = comps[ji + 1..comps.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();
    Some((module, pkg))
}

/// Number of leading components shared by every package path (the `com/x/y`
/// package root), so the tree isn't padded with it on every line.
fn common_prefix(dirs: &[&[String]]) -> usize {
    let Some(first) = dirs.first() else {
        return 0;
    };
    let mut n = 0;
    'outer: for i in 0..first.len() {
        for d in dirs.iter().skip(1) {
            if d.get(i) != Some(&first[i]) {
                break 'outer;
            }
        }
        n = i + 1;
    }
    n
}

/// Build a compact directory skeleton: module → package tree with per-package
/// file counts. This is the whole-workspace tier — the per-file symbol map is
/// too large to fit a multi-module project (see [`build_repo_map`]), so a bare
/// "orient" call gets the layout, and per-file symbols are fetched on demand via
/// a scoped `path`.
pub fn build_dir_tree(root: &Path, scope: Option<&str>) -> Result<String, String> {
    let scope = match scope {
        Some(s) if !s.trim().is_empty() => validate_workspace_path(root, s)?,
        _ => ".".to_string(),
    };

    let files = list_code_files(root, &scope)?;
    if files.is_empty() {
        return Ok("(no source files found in workspace)".to_string());
    }

    // Collect (module, package) per file, with a non-Maven fallback.
    let mut entries: Vec<(String, Vec<String>)> = Vec::new();
    let mut totals: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for f in &files {
        let (module, pkg) = split_module_package(f).unwrap_or_else(|| {
            let comps: Vec<&str> = f.split('/').collect();
            let module = comps
                .first()
                .map(|s| s.to_string())
                .unwrap_or_else(|| ".".to_string());
            let pkg = comps[1..comps.len().saturating_sub(1)]
                .iter()
                .map(|s| s.to_string())
                .collect();
            (module, pkg)
        });
        *totals.entry(module.clone()).or_insert(0) += 1;
        entries.push((module, pkg));
    }

    // Strip the shared package root (e.g. `com/dooya/cloud`) for scannability.
    let pkgs: Vec<&[String]> = entries.iter().map(|(_, p)| p.as_slice()).collect();
    let strip = common_prefix(&pkgs);

    // Build one package tree per module.
    let mut modules: std::collections::BTreeMap<String, DirNode> =
        std::collections::BTreeMap::new();
    for (module, pkg) in entries {
        let node = modules.entry(module).or_insert_with(DirNode::new);
        node.insert(&pkg[strip.min(pkg.len())..], 1);
    }

    let mut out = String::new();
    for (module, node) in &modules {
        let total = totals.get(module).copied().unwrap_or(0);
        let unit = if total == 1 { "file" } else { "files" };
        out.push_str(&format!("{module}/  ({total} {unit})\n"));
        for (name, child) in &node.children {
            let mut subtree = String::new();
            child.render(&mut subtree, name, 1);
            out.push_str(&subtree);
        }
        out.push('\n');
    }

    let module_label = if modules.len() == 1 { "module" } else { "modules" };
    let file_label = if files.len() == 1 { "file" } else { "files" };
    let result = format!(
        "Repository layout ({} {module_label}, {} {file_label}):\n\n{out}",
        modules.len(),
        files.len()
    );
    if result.chars().count() > MAX_DIR_TREE_CHARS {
        let mut truncated: String = result.chars().take(MAX_DIR_TREE_CHARS).collect();
        truncated.push_str("\n...(truncated)\n");
        return Ok(truncated);
    }
    Ok(result)
}

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &'static str {
        "repo_map"
    }

    fn description(&self) -> &'static str {
        "Produce a structural map of the codebase. With no `path`, returns a directory skeleton (module → package tree with file counts) to orient in a large workspace. Pass a workspace-relative `path` to get per-file symbols (classes, methods, fields, …) for that subtree. Call this FIRST."
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

    fn crate_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Create a temp dir holding `name: content` files, run the map, clean up.
    fn map_of_temp_files(tag: &str, files: &[(&str, &str)]) -> String {
        let root = std::env::temp_dir().join(format!("phimint_repomap_{tag}_test"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (name, content) in files {
            std::fs::write(root.join(name), content).unwrap();
        }
        let map = build_repo_map(&root, None).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        map
    }

    #[test]
    fn build_repo_map_extracts_known_symbols() {
        // Scope to a stable subdirectory: a full-repo map can exceed the output
        // budget (MAX_OUTPUT_CHARS) as the codebase grows, and symbol extraction
        // shouldn't be asserted against truncation order.
        let map = build_repo_map(&crate_root(), Some("src/tools")).unwrap();
        assert!(map.contains("ripgrep.rs"), "should list ripgrep.rs:\n{map}");
        assert!(map.contains("struct RipgrepTool"), "should extract struct:\n{map}");
        assert!(map.contains("fn search"), "should extract fn signature:\n{map}");
        assert!(!map.contains("target/"), "must not map build output:\n{map}");
    }

    #[test]
    fn build_repo_map_scopes_to_subdir() {
        let map = build_repo_map(&crate_root(), Some("src/tools")).unwrap();
        assert!(map.contains("ripgrep.rs"), "subdir map should list ripgrep.rs:\n{map}");
    }

    #[test]
    fn build_repo_map_extracts_java_symbols() {
        let map = map_of_temp_files(
            "java",
            &[(
                "Foo.java",
                "class Foo {\n  int bar(int x) { return x; }\n  private String name;\n}\n",
            )],
        );
        assert!(map.contains("Foo.java"), "{map}");
        assert!(map.contains("class Foo"), "{map}");
        assert!(map.contains("method bar"), "{map}");
        assert!(map.contains("field name"), "{map}");
    }

    #[test]
    fn build_repo_map_extracts_ts_symbols() {
        let map = map_of_temp_files(
            "ts",
            &[(
                "a.ts",
                "export class Widget {\n  render(): void {}\n  private id: number;\n}\n",
            )],
        );
        assert!(map.contains("a.ts"), "{map}");
        assert!(map.contains("class Widget"), "{map}");
        assert!(map.contains("method render"), "{map}");
    }

    #[test]
    fn build_repo_map_extracts_cpp_symbols() {
        let map = map_of_temp_files(
            "cpp",
            &[(
                "b.cpp",
                "class Shape {\npublic:\n  int area();\n};\nint global_fn(int x) { return x; }\n",
            )],
        );
        assert!(map.contains("b.cpp"), "{map}");
        assert!(map.contains("class Shape"), "{map}");
        assert!(map.contains("fn global_fn"), "{map}");
    }

    #[test]
    fn build_dir_tree_lists_modules_packages_and_strips_prefix() {
        let root = std::env::temp_dir().join("phimint_dirtree_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("app/src/main/java/com/x/controller")).unwrap();
        std::fs::create_dir_all(root.join("app/src/main/java/com/x/repo")).unwrap();
        std::fs::create_dir_all(root.join("client/src/main/java/com/x/io")).unwrap();
        std::fs::write(
            root.join("app/src/main/java/com/x/controller/Foo.java"),
            "class Foo {}",
        )
        .unwrap();
        std::fs::write(
            root.join("app/src/main/java/com/x/controller/Bar.java"),
            "class Bar {}",
        )
        .unwrap();
        std::fs::write(
            root.join("app/src/main/java/com/x/repo/Baz.java"),
            "class Baz {}",
        )
        .unwrap();
        std::fs::write(
            root.join("client/src/main/java/com/x/io/Qux.java"),
            "class Qux {}",
        )
        .unwrap();

        let tree = build_dir_tree(&root, None).unwrap();
        assert!(tree.contains("app/  (3 files)"), "{tree}");
        assert!(tree.contains("client/  (1 file)"), "{tree}");
        assert!(tree.contains("controller/ (2)"), "{tree}");
        assert!(tree.contains("repo/ (1)"), "{tree}");
        assert!(tree.contains("io/ (1)"), "{tree}");
        assert!(
            !tree.contains("com/x"),
            "common package prefix should be stripped:\n{tree}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
