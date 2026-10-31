//! File-tool face: when the file triad rides the menu at all.
//!
//! `read_file` / `list_files` / `write_file` exist to do file work. Code
//! cells — `code_cell` calls on the tool road, code steps on the act road —
//! do that work better: one cell batches what would be a chain of tool round
//! trips, intermediate data stays out of the context, and the effects land
//! on the code journal. So when the code road is actually live the triad is
//! dropped from the wire (the tool definitions stay registered — this is a
//! face, not an uninstall, and the cut is phimint-side only), and when the
//! code road cannot do the job the triad comes back:
//!
//! - **code cell live** → the triad is off the menu; file work rides code
//!   (`open()` / `pathlib`), `edit_file` and `search_content` stay.
//! - **code cell not live** → the triad is on the menu.
//!
//! "Live" is decided twice. At startup [`resolve_road`] probes for the
//! Python 3 interpreter the engine spawns (the code road needs one; nothing
//! auto-installs runtimes): no interpreter means the configured road degrades
//! to the no-engine shape — file tools loaded, no dead `code_cell` — and one
//! notice says why. Per turn, the face returns if the conversation shows the
//! code road dying on us (`history_has_code_env_failure`): the markers are
//! worker/env deaths ("could not spawn python worker", "no worker
//! available"), not ordinary cell errors — a `ValueError` is code failing,
//! not code infrastructure failing. The flip is sticky for the session and
//! derives from the messages themselves, so resume inherits it for free (the
//! same stateless trick as [`crate::browser_face`]).
//!
//! Prompt coherence: the code-road prompts teach file work via code cells and
//! never name the triad (see `agent::code_road_prompt`), so dropping the
//! tools never contradicts the prompt; a failure-driven flip only *adds* menu
//! entries the prompt omits, which is safe in the same direction as the
//! browser face's growth note.
//!
//! Sub-agents ride the same face at their spawn gate: when a child's kit
//! carries `code_cell`, `agent::child_multi_agent_config` cuts the read pair
//! from the child menu the same way this middleware cuts it from the host's
//! wire (a child model kept grinding `read_file` serially while `code_cell`
//! sat unused on the same kit — session 20261029_0ad90da2). The child cut is
//! static at startup — no per-turn flip there — and `PHIMINT_FILE_TOOL_FACE`
//! pins both faces back together.

use async_trait::async_trait;
use phi_agent::{AgentResult, ChatMessage, Middleware, PreLlmCtx};
use serde_json::Value;

use crate::config::CodeExecMode;

/// The file-operation triad code cells replace on the act road.
pub(crate) const FILE_TRIAD: [&str; 3] = ["read_file", "list_files", "write_file"];

/// Env-death markers of the code road as they ride the model-visible error
/// text (`ExecError::Worker` renders as `worker error: {inner}`). Only
/// infrastructure deaths flip the face — ordinary cell errors do not.
const ENV_FAILURE_MARKERS: [&str; 2] = ["could not spawn python worker", "no worker available"];

/// Ops rollback seam: `PHIMINT_FILE_TOOL_FACE=full` pins the triad on the
/// menu regardless of the code road — the behavior before the cut — so a bad
/// call can be undone in the field without a rebuild. Also consulted at the
/// sub-agent spawn gate (`agent::child_multi_agent_config`), which mirrors
/// this face onto the child kit.
pub(crate) fn triad_pinned() -> bool {
    std::env::var_os("PHIMINT_FILE_TOOL_FACE").is_some_and(|v| v == "full")
}

/// Drop the file triad from the wire tools when the code road owns file work.
/// Everything else — `edit_file`, `search_content`, a future tool from another
/// source — is untouched.
pub(crate) fn filter_file_tools(tools: &mut Vec<Value>, suppress: bool) {
    if !suppress {
        return;
    }
    tools.retain(|def| {
        let name = def
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str());
        !matches!(name, Some(n) if FILE_TRIAD.contains(&n))
    });
}

/// The text a message carries, for marker scans.
fn message_text(m: &ChatMessage) -> Option<&str> {
    match m {
        ChatMessage::System { content, .. }
        | ChatMessage::User { content, .. }
        | ChatMessage::Tool { content, .. } => Some(content.as_str()),
        ChatMessage::Assistant { content, .. } => content.as_deref(),
        ChatMessage::Custom { .. } => None,
    }
}

/// Whether the conversation shows the code road dying on its feet (worker /
/// interpreter death — see [`ENV_FAILURE_MARKERS`]). False-positive direction
/// is safe: a stray marker only puts tools back on the menu.
pub(crate) fn history_has_code_env_failure(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| {
        message_text(m).is_some_and(|t| ENV_FAILURE_MARKERS.iter().any(|marker| t.contains(marker)))
    })
}

/// The suppression decision for one LLM call: the code engine is live, the
/// field has not pinned the triad back, and no env death is on the record.
pub(crate) fn triad_suppressed(code_engine_live: bool, messages: &[ChatMessage]) -> bool {
    !triad_pinned() && code_engine_live && !history_has_code_env_failure(messages)
}

/// Per-turn file-tool face (see the module docs).
pub struct FileToolFaceMiddleware {
    /// Whether the code road's engine actually assembled this run (tool or
    /// act). False on the off road and in the kill-switch build — the triad
    /// stays.
    code_engine_live: bool,
}

impl FileToolFaceMiddleware {
    pub fn new(code_engine_live: bool) -> Self {
        Self { code_engine_live }
    }
}

#[async_trait]
impl Middleware for FileToolFaceMiddleware {
    async fn on_pre_llm(&self, ctx: &mut PreLlmCtx) -> AgentResult<()> {
        let suppress = triad_suppressed(self.code_engine_live, &ctx.messages);
        filter_file_tools(&mut ctx.tools, suppress);
        Ok(())
    }
}

// ── Startup: probe, degrade, notice ──────────────────────────────────────

static STARTUP_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The one startup notice about the code road, taken by the UI shell.
pub fn take_startup_notice() -> Option<String> {
    STARTUP_NOTICE.lock().ok().and_then(|mut g| g.take())
}

#[cfg(feature = "code-exec")]
fn set_startup_notice(msg: String) {
    if let Ok(mut guard) = STARTUP_NOTICE.lock() {
        *guard = Some(msg);
    }
}

/// What a degraded run tells the user: what was missing, what happened, and
/// both ways out (name an interpreter, or install one).
#[cfg(feature = "code-exec")]
const DEGRADE_NOTICE: &str = "code cells need Python 3 - none found (checked CODEXEC_PYTHON, \
    python3, python). Fell back to the file tools: read_file/list_files/write_file stay on the \
    menu; install Python 3 and restart to get the code road back";

/// One interpreter candidate answers `--version` with a Python 3.
#[cfg(feature = "code-exec")]
fn runs_python3(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .map(|out| {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            out.status.success() && text.contains("Python 3")
        })
        .unwrap_or(false)
}

/// Probe for the interpreter the engine will spawn. The engine resolves
/// `CODEXEC_PYTHON` then `python3` (codexec `workers/python/runtime.rs`); the
/// probe mirrors that, and when only the Windows-style `python` name works it
/// points `CODEXEC_PYTHON` at it so probe and spawn cannot disagree.
#[cfg(feature = "code-exec")]
pub fn probe_python() -> bool {
    if let Ok(p) = std::env::var("CODEXEC_PYTHON") {
        return runs_python3(&p);
    }
    if runs_python3("python3") {
        return true;
    }
    if runs_python3("python") {
        // SAFETY: startup seam, before any worker spawns or threads read the
        // environment — one process, one interpreter resolution.
        unsafe { std::env::set_var("CODEXEC_PYTHON", "python") };
        return true;
    }
    false
}

/// The degrade rule (hermetic core of [`resolve_road`]): a road that needs
/// the code cell with no interpreter behind it is not a road — resolve to the
/// no-engine shape (file tools on the menu, no dead `code_cell`) and say why.
/// A configured `off` is the user's own decision and gets no notice.
#[cfg(feature = "code-exec")]
pub(crate) fn degrade(mode: CodeExecMode, python_found: bool) -> (CodeExecMode, Option<String>) {
    if python_found || mode == CodeExecMode::Off {
        (mode, None)
    } else {
        (CodeExecMode::Off, Some(DEGRADE_NOTICE.to_string()))
    }
}

/// Resolve the configured road against this machine (startup seam): probe
/// for Python, degrade the road if it is missing, and stash the notice for
/// the UI shell. The hermetic rules are `degrade` (with an interpreter) and
/// the `resolved()` twins in [`crate::config`] (without the feature).
#[cfg(feature = "code-exec")]
pub fn resolve_road(mode: CodeExecMode) -> CodeExecMode {
    let (mode, notice) = degrade(mode, probe_python());
    if let Some(msg) = notice {
        set_startup_notice(msg);
    }
    mode
}

/// An engine-less binary has no code road to miss: always the off shape,
/// never a notice (disable layer ②).
#[cfg(not(feature = "code-exec"))]
pub fn resolve_road(mode: CodeExecMode) -> CodeExecMode {
    let _ = mode;
    CodeExecMode::Off
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::SessionId;

    fn tool_def(name: &str) -> Value {
        serde_json::json!({
            "type": "function",
            "function": { "name": name, "description": "d", "parameters": {} }
        })
    }

    fn menu() -> Vec<Value> {
        FILE_TRIAD
            .iter()
            .map(|n| tool_def(n))
            .chain([
                tool_def("edit_file"),
                tool_def("search_content"),
                tool_def("execute_command"),
            ])
            .collect()
    }

    fn names_of(tools: &[Value]) -> Vec<&str> {
        tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect()
    }

    fn ctx_with(messages: Vec<ChatMessage>) -> PreLlmCtx {
        PreLlmCtx {
            session_id: SessionId {
                id: 1,
                external_id: None,
            },
            messages,
            tools: menu(),
            emit_fn: None,
            turn_count: 1,
            max_turns: 10,
        }
    }

    fn observation(text: &str) -> ChatMessage {
        ChatMessage::user(format!("[observation]\n{text}"))
    }

    // ── Tool filtering ──────────────────────────────────────────────────

    #[test]
    fn suppression_drops_exactly_the_triad() {
        let mut tools = menu();
        filter_file_tools(&mut tools, true);
        assert_eq!(
            names_of(&tools),
            vec!["edit_file", "search_content", "execute_command"],
            "the triad goes, everything else stays"
        );
    }

    #[test]
    fn no_suppression_leaves_the_menu_alone() {
        let mut tools = menu();
        filter_file_tools(&mut tools, false);
        assert_eq!(names_of(&tools).len(), 6, "nothing dropped");
    }

    #[test]
    fn names_outside_the_wire_shape_pass_through() {
        // The filter only reads the OpenAI-style {"function": {"name": …}}
        // shape; anything else is not a managed name and stays.
        let mut tools = vec![
            tool_def("read_file"),
            serde_json::json!({"name": "read_file"}),
        ];
        filter_file_tools(&mut tools, true);
        assert_eq!(tools.len(), 1, "only the managed shape is filtered");
    }

    // ── Env-failure detection ───────────────────────────────────────────

    #[test]
    fn worker_death_markers_are_env_failures() {
        for marker in [
            "error: worker error: could not spawn python worker: No such file (os error 2)",
            "error: worker error: no worker available",
        ] {
            let msgs = vec![observation(marker)];
            assert!(
                history_has_code_env_failure(&msgs),
                "marker must flip the face: {marker}"
            );
        }
    }

    #[test]
    fn ordinary_cell_errors_do_not_flip_the_face() {
        let msgs = vec![observation(
            "error: error in block #0 (ValueError): bad input",
        )];
        assert!(
            !history_has_code_env_failure(&msgs),
            "a cell raising is code failing, not the code road dying"
        );
    }

    #[test]
    fn tool_results_carry_the_signal_too() {
        // The tool road surfaces the same worker errors as tool output.
        let msgs = vec![ChatMessage::Tool {
            tool_call_id: "c1".into(),
            name: Some("code_cell".into()),
            content: "worker error: could not spawn python worker: boom".into(),
            images: Vec::new(),
        }];
        assert!(history_has_code_env_failure(&msgs));
    }

    // ── The per-turn face ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_code_road_drops_the_triad_and_a_death_brings_it_back() {
        // Clean history: the code road owns file work, the triad is off.
        let mut ctx = ctx_with(vec![observation("namespace: x(int, 1)")]);
        FileToolFaceMiddleware::new(true)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            names_of(&ctx.tools),
            vec!["edit_file", "search_content", "execute_command"],
            "code cell live: no file triad on the wire"
        );

        // The same session after a worker death: the triad is back.
        let mut ctx = ctx_with(vec![observation(
            "error: worker error: could not spawn python worker: gone",
        )]);
        FileToolFaceMiddleware::new(true)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            names_of(&ctx.tools).len(),
            6,
            "code cell not successful: the file tools load back"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_engine_keeps_the_file_tools() {
        // Off road / kill-switch build / no session context: no engine
        // behind the code road, so nothing can own file work — the triad
        // stays.
        let mut ctx = ctx_with(vec![]);
        FileToolFaceMiddleware::new(false)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(names_of(&ctx.tools).len(), 6, "the triad stays");
    }

    // ── Startup rule (hermetic core) ────────────────────────────────────

    #[cfg(feature = "code-exec")]
    #[test]
    fn degrade_matrix() {
        // Python there: the configured road stands, no notice.
        for mode in [CodeExecMode::Act, CodeExecMode::Tool] {
            let (got, notice) = degrade(mode, true);
            assert_eq!(got, mode, "{mode:?} with Python");
            assert!(notice.is_none(), "{mode:?} with Python needs no notice");
        }
        // Python missing: the code roads degrade to the file-tool shape and
        // say so; the notice names both escape hatches.
        for mode in [CodeExecMode::Act, CodeExecMode::Tool] {
            let (got, notice) = degrade(mode, false);
            assert_eq!(got, CodeExecMode::Off, "{mode:?} without Python degrades");
            let msg = notice.expect("degrading must be announced");
            assert!(msg.contains("Python 3"), "{msg}");
            assert!(msg.contains("CODEXEC_PYTHON"), "{msg}");
            assert!(msg.contains("read_file/list_files/write_file"), "{msg}");
        }
        // Configured off is the user's decision — silent.
        let (got, notice) = degrade(CodeExecMode::Off, false);
        assert_eq!(got, CodeExecMode::Off);
        assert!(notice.is_none());
    }

    #[cfg(not(feature = "code-exec"))]
    #[test]
    fn without_the_engine_the_road_is_always_off() {
        for mode in [CodeExecMode::Act, CodeExecMode::Tool, CodeExecMode::Off] {
            assert_eq!(resolve_road(mode), CodeExecMode::Off, "{mode:?}");
        }
        assert!(take_startup_notice().is_none(), "nothing to announce");
    }

    #[cfg(feature = "code-exec")]
    #[test]
    fn missing_interpreter_names_fail_the_probe() {
        assert!(
            !runs_python3("phimint-no-such-interpreter"),
            "a name that does not resolve is not a Python"
        );
    }
}
