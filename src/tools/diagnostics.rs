//! Diagnostics 工具：拉取 LSP 诊断缓存（多 server）。thin `phi_agent::Tool`
//! shell——核心（文件收集 / 摘要格式化 / LSP 客户端与路由）在 `code-intel`。
//!
//! 与 `verify` 互补：`verify` 跑编译命令拿权威错误摘要，`diagnostics` 读各语言 LSP
//! server 的 `publishDiagnostics` 缓存，边写边报错（design §8.4）。两者都输出
//! `file:line:col  code  message` 摘要，agent 无需读原始编译输出。文件按语言路由
//! 到对应 server（rust-analyzer / typescript-language-server / clangd），没有 server
//! 的语言降级到 `verify`。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use code_intel::diagnostics::{collect_code_files, format_diagnostics};
use code_intel::lang;
use code_intel::lsp::{LspClient, LspManager};
use phi_agent::{AgentResult, Content, Tool, ToolContext, ToolMetadata};
use serde_json::{Value, json};

/// 等 LSP server 握手完成的上限（冷启动 ~1-2s，超时兜底）。
const STARTUP_TIMEOUT_MS: u64 = 15_000;
/// 同步后给 server 跑 save-time 检查（如 rust-analyzer checkOnSave）的沉降时间。
const SETTLE_MS: u64 = 2_000;

/// 拉取当前 LSP 诊断的工具（pull 式，多 server）。
pub struct DiagnosticsTool {
    manager: Arc<LspManager>,
    workspace_root: PathBuf,
}

impl DiagnosticsTool {
    pub fn new(manager: Arc<LspManager>, workspace_root: PathBuf) -> Self {
        Self {
            manager,
            workspace_root,
        }
    }
}

#[async_trait]
impl Tool for DiagnosticsTool {
    fn name(&self) -> &'static str {
        "diagnostics"
    }

    fn description(&self) -> &'static str {
        "Pull the current LSP diagnostics (errors/warnings) for the workspace as `file:line:col  code  message` lines, without recompiling. Files are routed to their language's server (rust-analyzer, typescript-language-server, clangd). Use after editing code for a fast error check; `verify` remains the authoritative full build check."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional workspace-relative file or directory to restrict diagnostics to. Defaults to the whole workspace."
                }
            },
            "required": []
        })
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name().to_string(),
            description: "Pull rust-analyzer diagnostics for the workspace.".to_string(),
            origin: "phimint".to_string(),
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

        // 1) 收集源码文件，按语言路由到对应 server 并 sync（阻塞 I/O → spawn_blocking）。
        let manager = self.manager.clone();
        let root = self.workspace_root.clone();
        let files_result = {
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                let (files, scope) = collect_code_files(&root, path.as_deref())?;
                // 用到的 server（按 Arc 指针去重，供后续 health/snapshot）。
                let mut clients: Vec<Arc<LspClient>> = Vec::new();
                let mut synced = 0usize;
                for f in &files {
                    let Some(client) = manager.client_for(f) else {
                        continue; // 该语言无 LSP server，降级到 verify。
                    };
                    let Some(language_id) = lang::lsp_language_id(&f.to_string_lossy()) else {
                        continue;
                    };
                    if let Ok(content) = std::fs::read_to_string(f) {
                        if !clients.iter().any(|c| Arc::ptr_eq(c, &client)) {
                            clients.push(client.clone());
                        }
                        client.sync(f, &content, language_id);
                        synced += 1;
                    }
                }
                Ok::<_, String>((files, synced, scope, clients))
            })
            .await
        };
        let (files, synced, scope, clients) = match files_result {
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
                "No source files found to diagnose in the workspace.".to_string(),
            )]);
        }
        if clients.is_empty() {
            return Ok(vec![Content::text(
                "No LSP server is registered for these files' languages - run `verify` instead."
                    .to_string(),
            )]);
        }

        tracing::info!(files = synced, "diagnostics: synced files to LSP servers");

        // 2) 等各 server 握手完成；任一失败则明确报告而非空说「无诊断」。
        wait_ready(&clients).await;
        if let Some(e) = first_health_error(&clients) {
            return Ok(vec![Content::text(format!(
                "[Error]: diagnostics unavailable - {e}. Run `verify` instead."
            ))]);
        }

        // 3) 沉降：给 save-time 检查时间跑 check + publish。
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        // 4) 合并各 server 快照 + 按范围过滤 + 格式化。
        let snapshot: Vec<_> = clients.iter().flat_map(|c| c.snapshot()).collect();
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

/// 等所有 server 握手完成（任一启动失败或超时则提前返回，不空等）。
async fn wait_ready(clients: &[Arc<LspClient>]) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(STARTUP_TIMEOUT_MS);
    loop {
        if clients.iter().all(|c| c.health().is_ok()) {
            return;
        }
        if clients.iter().any(|c| c.failed_error().is_some()) {
            return; // 有 server 启动失败，错误已记录。
        }
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// 第一个 health 错误（所有 server 就绪则为 None）。
fn first_health_error(clients: &[Arc<LspClient>]) -> Option<String> {
    clients.iter().find_map(|c| c.health().err())
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

    /// 空路由表（无任何 server）的 manager：`client_for` 恒为 None，
    /// 惰性启动保证不会拉起真实 LSP 进程。
    fn manager(root: &std::path::Path) -> Arc<LspManager> {
        Arc::new(LspManager::new(
            root.to_path_buf(),
            code_intel::lsp::ClientInfo {
                name: "phimint-test".into(),
                version: None,
            },
            |_path| None,
        ))
    }

    #[test]
    fn metadata_carries_identity() {
        let tool = DiagnosticsTool::new(manager(std::env::temp_dir().as_path()), std::env::temp_dir());
        assert_eq!(tool.name(), "diagnostics");
        let md = tool.metadata();
        assert_eq!(md.name, "diagnostics");
        assert_eq!(md.origin, "phimint");
        assert!(tool.schema()["properties"].get("path").is_some());
    }

    #[tokio::test]
    async fn empty_workspace_reports_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let tool = DiagnosticsTool::new(manager(dir.path()), dir.path().to_path_buf());

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        assert!(text(&out).contains("No source files found"), "{}", text(&out));
    }

    #[tokio::test]
    async fn no_server_degrades_to_verify_hint() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn f() {}\n").unwrap();
        let tool = DiagnosticsTool::new(manager(dir.path()), dir.path().to_path_buf());

        let out = tool.call(&json!({}), &ctx()).await.unwrap();
        assert!(
            text(&out).contains("No LSP server is registered"),
            "{}",
            text(&out)
        );
        assert!(text(&out).contains("verify"), "{}", text(&out));
    }
}
