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
//! 「必须验」策略——那是 phimint 的强需求，其他业务不需要（design §8.3）。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use phi_agent::{AgentError, AgentResult, Middleware, PostLlmCtx, UserMessageCtx};
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
        Some(p) => crate::lang::is_code_path(&p),
        None => true,
    }
}

/// 闸门配置。
pub struct VerifyEnforcementConfig {
    /// 最多连续 nudge 几次；之后直接失败，结束本轮。
    pub max_nudges: usize,
    /// 注入给 agent 的提醒（User 角色，逼它先跑 verify）。
    pub nudge_message: String,
    /// 本轮是否可能发生写入。`deny` 模式写工具一律被拒，dirty 永不成立，
    /// 闸门整体关闭，避免误伤只读 agent。
    pub writes_possible: bool,
}

impl Default for VerifyEnforcementConfig {
    fn default() -> Self {
        Self {
            max_nudges: 3,
            nudge_message: "STOP. You edited code/config files but did not run `verify`. \
                 You MUST call `verify` NOW before saying done. \
                 Do not write any more text — just call verify."
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
                    st.nudges = 0; // verify 运行后重置 nudge 计数
                }
            }
            (st.dirty, st.nudges)
        };

        // 2) 只有「纯文本、想结束、且有未验证的改动」才需要拦。
        if ctx.is_tool_call || ctx.full_text.is_empty() || !dirty {
            return Ok(());
        }

        // 3) 超过上限 → 直接失败，结束本轮。
        if nudges >= self.config.max_nudges {
            tracing::warn!(
                session_id = ctx.session_id.id,
                max_nudges = self.config.max_nudges,
                "verify gate: max nudges reached, failing turn"
            );
            return Err(AgentError::Internal(
                "verify gate: refused to run verify after multiple attempts".to_string(),
            ));
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
    use phi_agent::{FinishReason, SessionId};

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
            finish_reason: FinishReason::Stop,
        }
    }

    #[test]
    fn config_defaults() {
        let cfg = VerifyEnforcementConfig::default();
        assert_eq!(cfg.max_nudges, 3);
        assert!(cfg.writes_possible);
        assert!(cfg.nudge_message.contains("verify"));
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
    async fn multilang_code_edit_marks_dirty() {
        for path in ["src/Foo.java", "src/a.ts", "src/b.cpp"] {
            let mw = VerifyEnforcementMiddleware::new(VerifyEnforcementConfig::default());

            let mut edit = ctx(true, "", vec![write_call("edit_file", path)]);
            mw.on_post_llm(&mut edit).await.unwrap();

            let mut done = ctx(false, "done", vec![]);
            mw.on_post_llm(&mut done).await.unwrap();
            assert!(done.skip_push, "{path} edit must trigger the gate");
        }
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
        assert!(crate::lang::is_code_path("src/lib.rs"));
        assert!(crate::lang::is_code_path("build.rs"));
        assert!(crate::lang::is_code_path("Cargo.toml"));
        assert!(crate::lang::is_code_path("Cargo.lock"));
        assert!(crate::lang::is_code_path("docs/Cargo.toml"));
        assert!(crate::lang::is_code_path("Foo.java"));
        assert!(crate::lang::is_code_path("src/a.ts"));
        assert!(crate::lang::is_code_path("src/b.tsx"));
        assert!(crate::lang::is_code_path("src/c.cpp"));
        assert!(!crate::lang::is_code_path("README.md"));
        assert!(!crate::lang::is_code_path("docs/notes.txt"));
        assert!(!crate::lang::is_code_path("src/"));
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
    async fn fails_after_max_nudges() {
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

        // 第二次：直接失败。
        let mut second = ctx(false, "done", vec![]);
        let result = mw.on_post_llm(&mut second).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("refused to run verify"));
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
