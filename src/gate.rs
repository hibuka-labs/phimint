//! 强制 verify 闸门（Phase 6a）：consumer-side middleware，零改框架。
//!
//! 产品招牌「永远不把编不过的代码交给你」的强制半截。agent 在本轮（一条
//! 用户消息内）动过代码文件（`write_file` / `edit_file`）却还没跑过 `verify`
//! 或 `merge` 时，把它试图「报 done」的纯文本回复压掉（`skip_push`），注入
//! 一句提醒逼它先验证。连续 nudge `max_nudges` 次仍不验就降级：不再拦截，
//! 只在最终回复里追加一行「⚠️ Unverified」警示，绝不把 agent 卡死（死锁
//! 逃生口，呼应「绝对化可能引发的 bug」）。
//!
//! 只依赖框架已有的中性钩子 `Middleware::on_post_llm`（`skip_push` /
//! `follow_up_message`）与 `on_user_message`（每轮重置），不向框架塞任何
//! 「必须验」策略——那是 phiforge 的强需求，其他业务不需要（design §8.3）。

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use phi_agent::{AgentResult, Middleware, PostLlmCtx, UserMessageCtx};
use serde_json::Value;

/// 会「弄脏」工作区的写工具。
const DIRTY_TOOLS: [&str; 2] = ["write_file", "edit_file"];

/// 会「验证」工作区、清空 dirty 的工具（`merge` 内部跑 `cargo check`）。
const VERIFY_TOOLS: [&str; 2] = ["verify", "merge"];

/// 一次写工具调用是否落在「影响 `cargo check` 结果」的文件上。
///
/// 只对代码文件置 dirty：写 README/docs 不能把工程写坏，不该触发强制验证。
/// 解析不出路径时保守返回 `true`——宁可多验一次，也不漏验。
fn is_code_edit(args: &str) -> bool {
    let path = serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| v.get("path").and_then(Value::as_str).map(str::to_owned));
    match path {
        Some(p) => is_code_path(&p),
        None => true,
    }
}

/// 路径是否影响 cargo 编译：`.rs` 源文件，或 Cargo 清单/锁文件。
fn is_code_path(path: &str) -> bool {
    if path.ends_with(".rs") {
        return true;
    }
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path);
    name == "Cargo.toml" || name == "Cargo.lock"
}

/// 闸门配置。
pub struct VerifyEnforcementConfig {
    /// 最多连续 nudge 几次；之后降级为「⚠️ Unverified」警示而非继续拦截。
    pub max_nudges: usize,
    /// 注入给 agent 的提醒（User 角色，逼它先跑 verify）。
    pub nudge_message: String,
    /// 降级时追加到最终回复（用户可见）的警示。
    pub degrade_warning: String,
    /// 本轮是否可能发生写入。`deny` 模式写工具一律被拒，dirty 永不成立，
    /// 闸门整体关闭，避免误伤只读 agent。
    pub writes_possible: bool,
}

impl Default for VerifyEnforcementConfig {
    fn default() -> Self {
        Self {
            max_nudges: 3,
            nudge_message: "CRITICAL: You edited files this turn but did not run `verify`. Run \
                 `verify` now and act on its result before reporting done — never finish on \
                 unverified edits."
                .to_string(),
            degrade_warning: "\n\n⚠️ Unverified: files were edited this turn but `verify` was \
                 never run — the result may not compile."
                .to_string(),
            writes_possible: true,
        }
    }
}

/// 本轮闸门状态：`on_user_message` 重置，`on_post_llm` 内累进。
#[derive(Default)]
struct GateState {
    dirty: bool,
    nudges: usize,
}

/// 强制 verify 闸门（见模块文档）。
pub struct VerifyEnforcementMiddleware {
    config: VerifyEnforcementConfig,
    state: Arc<Mutex<GateState>>,
}

impl VerifyEnforcementMiddleware {
    pub fn new(config: VerifyEnforcementConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(GateState::default())),
        }
    }
}

#[async_trait]
impl Middleware for VerifyEnforcementMiddleware {
    /// 每条用户消息 = 一轮新的「编辑 → 验证」周期。
    async fn on_user_message(&self, _ctx: &mut UserMessageCtx) -> AgentResult<()> {
        let mut st = self.state.lock().expect("gate state lock poisoned");
        st.dirty = false;
        st.nudges = 0;
        Ok(())
    }

    async fn on_post_llm(&self, ctx: &mut PostLlmCtx) -> AgentResult<()> {
        if !self.config.writes_possible {
            return Ok(());
        }

        // 1) 按本轮响应里的工具调用更新 dirty：写工具置位，验证工具清位。
        let (dirty, nudges) = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            for (_id, name, args) in &ctx.tool_calls {
                if DIRTY_TOOLS.contains(&name.as_str()) && is_code_edit(args) {
                    st.dirty = true;
                } else if VERIFY_TOOLS.contains(&name.as_str()) {
                    st.dirty = false;
                }
            }
            (st.dirty, st.nudges)
        };

        // 2) 只有「纯文本、想结束、且有未验证的改动」才需要拦。
        if ctx.is_tool_call || ctx.full_text.is_empty() || !dirty {
            return Ok(());
        }

        // 3) 超过上限 → 降级：放行，追加警示（逃生口，绝不卡死）。
        if nudges >= self.config.max_nudges {
            ctx.full_text.push_str(&self.config.degrade_warning);
            tracing::warn!(
                session_id = ctx.session_id.id,
                max_nudges = self.config.max_nudges,
                "verify gate: max nudges reached, degrading to warning"
            );
            return Ok(());
        }

        // 4) 否决这次回复，注入提醒，逼循环再走一轮。
        let new_nudges = {
            let mut st = self.state.lock().expect("gate state lock poisoned");
            st.nudges += 1;
            st.nudges
        };
        tracing::info!(
            session_id = ctx.session_id.id,
            nudge = new_nudges,
            "verify gate: suppressing unverified done, injecting verify nudge"
        );
        ctx.skip_push = true;
        ctx.follow_up_message = Some(self.config.nudge_message.clone());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::SessionId;

    /// 造一条 `(id, name, args)` 工具调用（args 默认 `{}`）。
    fn call(name: &str) -> (String, String, String) {
        call_with_args(name, "{}")
    }

    /// 造一条指定 args 的工具调用。
    fn call_with_args(name: &str, args: &str) -> (String, String, String) {
        (format!("call_{name}"), name.to_string(), args.to_string())
    }

    /// 造一条带 `path` 的写工具调用。
    fn write_call(tool: &str, path: &str) -> (String, String, String) {
        call_with_args(tool, &format!(r#"{{"path": "{path}"}}"#))
    }

    /// 造一个 `PostLlmCtx`，字段除 `is_tool_call`/`full_text`/`tool_calls` 外取默认。
    fn ctx(
        is_tool_call: bool,
        full_text: &str,
        tool_calls: Vec<(String, String, String)>,
    ) -> PostLlmCtx {
        PostLlmCtx {
            session_id: SessionId::new(1),
            full_text: full_text.to_string(),
            is_tool_call,
            tool_calls,
            available_tools: vec![],
            turn_count: 1,
            total_tool_calls: 0,
            nudge_count: 0,
            turn_tool_calls: 0,
            skip_push: false,
            follow_up_message: None,
        }
    }

    #[test]
    fn config_defaults() {
        let cfg = VerifyEnforcementConfig::default();
        assert_eq!(cfg.max_nudges, 3);
        assert!(cfg.writes_possible);
        assert!(cfg.nudge_message.contains("verify"));
        assert!(cfg.degrade_warning.contains("Unverified"));
    }

    #[tokio::test]
    async fn vetoes_unverified_done() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());
        let expected = VerifyEnforcementConfig::default().nudge_message;

        // write_file 标记 dirty（工具调用本身不被拦）。
        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // 随后 text-only「报 done」被否决。
        let mut done = ctx(false, "All done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
        assert_eq!(done.follow_up_message, Some(expected));
    }

    #[tokio::test]
    async fn edit_file_also_marks_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("edit_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
    }

    #[tokio::test]
    async fn doc_edit_does_not_mark_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![write_call("write_file", "README.md")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "wrote README, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push, "doc-only edits must not trigger the gate");
    }

    #[tokio::test]
    async fn code_edit_marks_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![write_call("write_file", "src/lib.rs")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "wrote src/lib.rs, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
    }

    #[tokio::test]
    async fn cargo_manifest_marks_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![write_call("write_file", "Cargo.toml")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "bumped dep, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(done.skip_push);
    }

    #[test]
    fn is_code_path_classifies() {
        assert!(is_code_path("src/lib.rs"));
        assert!(is_code_path("build.rs"));
        assert!(is_code_path("Cargo.toml"));
        assert!(is_code_path("Cargo.lock"));
        assert!(is_code_path("docs/Cargo.toml"));
        assert!(!is_code_path("README.md"));
        assert!(!is_code_path("docs/notes.txt"));
        assert!(!is_code_path("src/"));
    }

    #[test]
    fn missing_path_conservatively_marks_dirty() {
        // 解析不出 path 时宁可多验一次。
        assert!(is_code_edit("{}"));
        assert!(is_code_edit("not json"));
    }

    #[tokio::test]
    async fn verify_clears_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut vfy = ctx(true, "", vec![call("verify")]);
        mw.on_post_llm(&mut vfy).await.unwrap();

        let mut done = ctx(false, "verified, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
        assert!(done.follow_up_message.is_none());
    }

    #[tokio::test]
    async fn merge_clears_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // merge 内部跑 cargo check，等同验证。
        let mut mg = ctx(true, "", vec![call("merge")]);
        mw.on_post_llm(&mut mg).await.unwrap();

        let mut done = ctx(false, "merged, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }

    #[tokio::test]
    async fn only_write_tools_mark_dirty() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        // 只读/搜索工具不得触发闸门。
        let mut read = ctx(true, "", vec![call("read_file"), call("search_content")]);
        mw.on_post_llm(&mut read).await.unwrap();

        let mut done = ctx(false, "just read, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push, "read-only tools must not trigger the gate");
    }

    #[tokio::test]
    async fn no_veto_when_clean() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        // 本轮没有任何写入，纯文本 done 不拦。
        let mut done = ctx(false, "nothing changed, done.", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }

    #[tokio::test]
    async fn no_veto_on_tool_call() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // 还在干活（工具调用）时不拦，哪怕 dirty。
        let mut work = ctx(true, "", vec![call("read_file")]);
        mw.on_post_llm(&mut work).await.unwrap();
        assert!(!work.skip_push);
    }

    #[tokio::test]
    async fn no_veto_on_empty_text() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // 空文本不是「报 done」，不拦（与 ToolEnforcementMiddleware 一致）。
        let mut empty = ctx(false, "", vec![]);
        mw.on_post_llm(&mut empty).await.unwrap();
        assert!(!empty.skip_push);
    }

    #[tokio::test]
    async fn degrades_to_warning_after_max_nudges() {
        let config = VerifyEnforcementConfig {
            max_nudges: 1,
            ..VerifyEnforcementConfig::default()
        };
        let mw = VerifyEnforcementMiddleware::new(config);

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // 第一次 nudge（到上限）。
        let mut first = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut first).await.unwrap();
        assert!(first.skip_push);

        // 第二次：降级——放行并追加警示，不再拦。
        let mut second = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut second).await.unwrap();
        assert!(!second.skip_push);
        assert!(second.follow_up_message.is_none());
        assert!(second.full_text.contains("Unverified"));
    }

    #[tokio::test]
    async fn on_user_message_resets_state() {
        let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

        // 编辑标记 dirty。
        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        // 新用户消息开始新周期。
        let mut u = UserMessageCtx {
            session_id: SessionId::new(1),
            user_input: "hi".into(),
        };
        mw.on_user_message(&mut u).await.unwrap();

        // 新周期内没有编辑，done 不拦。
        let mut done = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }

    #[tokio::test]
    async fn disabled_when_writes_not_possible() {
        let config = VerifyEnforcementConfig {
            writes_possible: false,
            ..VerifyEnforcementConfig::default()
        };
        let mw = VerifyEnforcementMiddleware::new(config);

        let mut edit = ctx(true, "", vec![call("write_file")]);
        mw.on_post_llm(&mut edit).await.unwrap();

        let mut done = ctx(false, "done", vec![]);
        mw.on_post_llm(&mut done).await.unwrap();
        assert!(!done.skip_push);
    }
}
