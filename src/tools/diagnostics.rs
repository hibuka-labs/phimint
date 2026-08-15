//! Diagnostics 工具（Phase 6b）：拉取 rust-analyzer（LSP）诊断缓存。
//!
//! 与 `verify` 互补：`verify` 跑 `cargo check` 拿权威错误摘要，`diagnostics` 读
//! rust-analyzer 的 `publishDiagnostics` 缓存，边写边报错（design §8.4）。两者都
//! 输出 `file:line:col  code  message` 摘要，agent 无需读原始编译输出。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

use super::validate_workspace_path;
use crate::lsp::{DiagnosticEntry, LspClient, Severity};

/// 等 rust-analyzer 握手完成的上限（冷启动 ~1-2s，超时兜底）。
const STARTUP_TIMEOUT_MS: u64 = 15_000;
/// 同步后给 rust-analyzer 跑 checkOnSave 的沉降时间。
const SETTLE_MS: u64 = 2_000;
/// 摘要截断上限（对齐 `verify` 的 `MAX_SUMMARY_CHARS`）。
const MAX_SUMMARY_CHARS: usize = 4000;

/// 拉取当前 rust-analyzer 诊断的工具（pull 式）。
pub struct DiagnosticsTool {
    client: Arc<LspClient>,
    workspace_root: PathBuf,
}

impl DiagnosticsTool {
    pub fn new(client: Arc<LspClient>, workspace_root: PathBuf) -> Self {
        Self {
            client,
            workspace_root,
        }
    }
}

/// 收集要诊断的 `.rs` 文件。
///
/// `path`（可选，workspace 相对）收敛到单个文件或目录；省略则扫整个 workspace，
/// 跳过 `target/`、`node_modules/` 和隐藏目录。返回 `(文件列表, 范围基准路径)`，
/// 范围用于最后过滤诊断快照。
fn collect_rs_files(root: &Path, path: Option<&str>) -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
    let scope = match path {
        Some(p) if !p.trim().is_empty() => {
            let rel = validate_workspace_path(root, p)?;
            Some(root.join(rel))
        }
        _ => None,
    };
    let base = scope.clone().unwrap_or_else(|| root.to_path_buf());

    let mut files = Vec::new();
    if base.is_file() {
        if base.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(base);
        }
        return Ok((files, scope));
    }
    walk_rs_files(&base, &mut files);
    files.sort();
    Ok((files, scope))
}

/// 递归收集 `.rs` 文件，跳过 build 产物与隐藏目录。
fn walk_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if name == "target" || name == "node_modules" {
                continue;
            }
            walk_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// 把诊断缓存格式化成 `verify` 风格的摘要。
///
/// 路径以 workspace 相对形式呈现；只保留 error / warning（rust-analyzer 还会发
/// hint/info，如「expected due to this」，对「编不过」无意义，舍弃）；无 error/warning
/// 时返回单行「无诊断」。
pub fn format_diagnostics(entries: &[(PathBuf, Vec<DiagnosticEntry>)], root: &Path) -> String {
    let mut lines = Vec::new();
    let (mut errors, mut warnings) = (0usize, 0usize);

    for (file, diags) in entries {
        let rel = file.strip_prefix(root).unwrap_or(file);
        for d in diags {
            let code = d.code.clone().unwrap_or_else(|| d.severity.label().to_string());
            match d.severity {
                Severity::Error => {
                    errors += 1;
                    lines.push(format!(
                        "  {}:{}:{}  {}  {}",
                        rel.display(),
                        d.line,
                        d.column,
                        code,
                        d.message
                    ));
                }
                Severity::Warning => {
                    warnings += 1;
                    lines.push(format!(
                        "  {}:{}:{}  {}  {}",
                        rel.display(),
                        d.line,
                        d.column,
                        code,
                        d.message
                    ));
                }
                Severity::Information | Severity::Hint => {}
            }
        }
    }

    if errors == 0 && warnings == 0 {
        return "✓ no diagnostics — rust-analyzer reports no errors or warnings".to_string();
    }

    let mut out = String::new();
    let mut head = String::new();
    if errors > 0 {
        head.push_str(&format!("{errors} error(s)"));
    }
    if warnings > 0 {
        if !head.is_empty() {
            head.push_str(", ");
        }
        head.push_str(&format!("{warnings} warning(s)"));
    }
    out.push_str(&format!("{head}:\n"));
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }

    if out.chars().count() > MAX_SUMMARY_CHARS {
        let mut truncated: String = out.chars().take(MAX_SUMMARY_CHARS).collect();
        truncated.push_str("...(truncated)\n");
        return truncated;
    }
    out
}

#[async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &'static str {
        "diagnostics"
    }

    fn description(&self) -> &'static str {
        "Pull the current rust-analyzer diagnostics (errors/warnings) for the workspace as `file:line:col  code  message` lines, without recompiling. Use after editing Rust code for a fast error check; `verify` remains the authoritative full `cargo check`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative .rs file or directory to restrict diagnostics to. Defaults to the whole workspace."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Pull rust-analyzer diagnostics for the workspace.".to_string(),
            origin: "phiforge".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            requirements: vec![],
        }
    }

    async fn call(&self, args: &Value, _ctx: &ToolContext) -> AgentResult<Vec<Content>> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);

        // 1) 收集 .rs 文件 + 读内容 + 推给 rust-analyzer（阻塞 I/O → spawn_blocking）。
        let client = self.client.clone();
        let root = self.workspace_root.clone();
        let files_result = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                let (files, scope) = collect_rs_files(&root, path.as_deref())?;
                let mut synced = 0usize;
                for f in &files {
                    if let Ok(content) = std::fs::read_to_string(f) {
                        client.sync(f, &content);
                        synced += 1;
                    }
                }
                Ok::<_, String>((files, synced, scope))
            })
            .await
        };
        let (files, synced, scope) = match files_result {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Ok(vec![Content::text(format!("[Error]: {e}"))]),
            Err(e) => {
                return Ok(vec![Content::text(format!(
                    "[Error]: diagnostics task failed: {e}"
                ))])
            }
        };
        if files.is_empty() {
            return Ok(vec![Content::text(
                "No .rs files found to diagnose in the workspace.".to_string(),
            )]);
        }

        tracing::info!(files = synced, "diagnostics: synced files to rust-analyzer");

        // 2) 等握手完成；失败则明确报告而非空说「无诊断」。
        self.wait_ready().await;
        if let Err(e) = self.client.health() {
            return Ok(vec![Content::text(format!(
                "[Error]: diagnostics unavailable — {e}. Run `verify` (cargo check) instead."
            ))]);
        }

        // 3) 沉降：给 checkOnSave 时间跑 cargo check + publish。
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        // 4) 读缓存 + 按范围过滤 + 格式化。
        let snapshot = self.client.snapshot();
        let filtered: Vec<_> = match &scope {
            Some(sp) => snapshot
                .into_iter()
                .filter(|(p, _)| p.starts_with(sp))
                .collect(),
            None => snapshot,
        };
        let summary = format_diagnostics(&filtered, &root);
        Ok(vec![Content::text(summary)])
    }
}

impl DiagnosticsTool {
    /// 等 rust-analyzer 握手完成，启动失败则立即返回（不空等）。
    async fn wait_ready(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(STARTUP_TIMEOUT_MS);
        loop {
            if self.client.health().is_ok() {
                return;
            }
            if self.client.failed_error().is_some() {
                return; // 启动失败，错误已记录。
            }
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{DiagnosticEntry, Severity};

    fn entry(severity: Severity, line: u32, column: u32, code: Option<&str>, message: &str) -> DiagnosticEntry {
        DiagnosticEntry {
            severity,
            line,
            column,
            message: message.to_string(),
            code: code.map(str::to_string),
        }
    }

    #[test]
    fn format_empty_is_a_clean_line() {
        let s = format_diagnostics(&[], Path::new("/ws"));
        assert!(s.contains("no diagnostics"), "{s}");
    }

    #[test]
    fn format_groups_errors_and_warnings() {
        let entries = vec![
            (
                PathBuf::from("/ws/src/main.rs"),
                vec![
                    entry(Severity::Error, 3, 5, Some("E0425"), "unresolved name"),
                    entry(Severity::Warning, 7, 1, None, "unused variable"),
                ],
            ),
            (
                PathBuf::from("/ws/src/lib.rs"),
                vec![entry(Severity::Error, 1, 1, Some("E0308"), "mismatched types")],
            ),
        ];
        let s = format_diagnostics(&entries, Path::new("/ws"));
        assert!(s.starts_with("2 error(s), 1 warning(s):"), "{s}");
        assert!(s.contains("src/main.rs:3:5  E0425  unresolved name"), "{s}");
        assert!(s.contains("src/main.rs:7:1  warning  unused variable"), "{s}");
        assert!(s.contains("src/lib.rs:1:1  E0308  mismatched types"), "{s}");
    }

    #[test]
    fn format_skips_empty_files() {
        let entries = vec![(PathBuf::from("/ws/src/main.rs"), vec![])];
        let s = format_diagnostics(&entries, Path::new("/ws"));
        assert!(s.contains("no diagnostics"), "{s}");
    }

    #[test]
    fn collect_rs_files_finds_sources_and_skips_target() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let (files, _) = collect_rs_files(&root, None).unwrap();
        assert!(files.iter().any(|f| f.ends_with("src/main.rs")), "{files:?}");
        for f in &files {
            assert!(f.extension().and_then(|e| e.to_str()) == Some("rs"), "{f:?}");
            assert!(!f.to_string_lossy().contains("/target/"), "must skip target: {f:?}");
        }
    }

    #[test]
    fn collect_scopes_to_single_file() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let (files, scope) = collect_rs_files(&root, Some("src/main.rs")).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with("src/main.rs"));
        assert!(scope.unwrap().ends_with("src/main.rs"));
    }
}
