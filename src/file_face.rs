//! File-op faces: which file-work vehicles the wire carries.
//!
//! Two roads do file work and both ride the menu by default: the file triad
//! (`read_file` / `list_files` / `write_file` — single-file work, line
//! numbers, pagination, small arguments) and the code road (`code_cell`
//! calls on the tool road, code steps on the act road — chains of file
//! operations, intermediates kept in the interpreter). The system prompt
//! teaches the dispatch rule between them (see `agent::code_road_prompt`);
//! this module only manages the two faces when one of the roads cannot do
//! its job:
//!
//! - **code road dead** → `code_cell` drops from the wire: the menu stops
//!   offering a vehicle that can no longer run (the triad is already on the
//!   menu — file work never loses its floor).
//! - **`PHIMINT_FILE_TOOL_FACE=code`** (ops rollback to the M3b face) →
//!   code owns file work: the triad drops while the code road is alive and
//!   comes back when it dies.
//!
//! Liveness is decided twice. At startup [`resolve_road`] probes for the
//! Python 3 interpreter the engine spawns (the code road needs one; nothing
//! auto-installs runtimes): no interpreter means the configured road degrades
//! to the no-engine shape — no dead `code_cell` on the menu, no code-cell
//! teaching in the prompt — and one notice says why. Per turn, `code_road_dead`
//! reads the conversation: the markers are worker/env deaths ("could not
//! spawn python worker", "no worker available"), not ordinary cell errors —
//! a `ValueError` is code failing, not code infrastructure failing.
//!
//! The per-turn scan is stateless (the same trick as [`crate::browser_face`]):
//! the record itself decides, so there is no side state to desync from
//! resume. Two properties fall out of the scan shape:
//!
//! - **Carriers are the code road's own surfaces** — a `code_cell` tool
//!   result (tool road) or an `[observation]` line (act road). A
//!   `search_content` hit on this very file's marker strings is not the road
//!   dying and cuts nothing.
//! - **A fresh user turn is a fresh look.** The engine respawns its worker
//!   on every call, so re-offering `code_cell` is how a transient death
//!   heals; a permanent one costs one failed call per user turn and then the
//!   menu agrees with the error for the rest of it. Resume heals the same
//!   way: the user speaks, the window resets.
//!
//! Prompt coherence, one direction only: the prompts name both vehicles on
//! every road (the dispatch rule needs both), so adding menu entries never
//! contradicts them. The cut direction has a known gap — a dead code road
//! leaves the prompt still naming `code_cell` while the wire drops it. The
//! model has seen the death error by then (that is where the marker came
//! from), and a hallucinated call fails once and reads as the same evidence.
//!
//! Sub-agents ride the same menu both ways (tool road): their kit keeps the
//! triad and carries `code_cell` in a read-only resident session of their
//! own (`agent::child_multi_agent_config`). The child kit is static at
//! spawn — no per-turn flip there — so a dead code road mid-run degrades a
//! child's cell call into a readable error, never a silent hole.

use async_trait::async_trait;
use phi_agent::{AgentResult, ChatMessage, Middleware, PreLlmCtx};
use serde_json::Value;

use crate::config::CodeExecMode;

/// The file-operation triad: the single-file vehicle.
pub(crate) const FILE_TRIAD: [&str; 3] = ["read_file", "list_files", "write_file"];

/// The code road's tool name on the tool road (wire name, agent-works).
const CODE_CELL_TOOL: &str = "code_cell";

/// Env-death markers of the code road as they ride the model-visible error
/// text (`ExecError::Worker` renders as `worker error: {inner}`). Only
/// infrastructure deaths flip the face — ordinary cell errors do not.
const ENV_FAILURE_MARKERS: [&str; 2] = ["could not spawn python worker", "no worker available"];

/// The observation line prefix the act road renders for every step result
/// (agent-works `code_act`) — the act road's carrier for the scan below.
const OBSERVATION_PREFIX: &str = "[observation]";

/// Ops rollback seam: `PHIMINT_FILE_TOOL_FACE=code` restores the M3b face —
/// code owns file work and the triad drops from the menu while the code road
/// is alive. Unset (or `full`, the old spelling for "triad on", which is now
/// just the default) keeps both vehicles on the menu. Read once at build and
/// handed to [`FileToolFaceMiddleware`] so the face cannot flip mid-run.
pub(crate) fn code_owns_file_tools() -> bool {
    std::env::var_os("PHIMINT_FILE_TOOL_FACE").is_some_and(|v| v == "code")
}

/// Drop the named tools from the wire tools. Everything else — `edit_file`,
/// `search_content`, a future tool from another source — is untouched. The
/// tool definitions stay registered: a face is not an uninstall.
pub(crate) fn drop_tools(tools: &mut Vec<Value>, names: &[&str]) {
    tools.retain(|def| {
        let name = def
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str());
        !matches!(name, Some(n) if names.contains(&n))
    });
}

/// One message's code-road signal: `Some(alive)` when the message is the
/// code road's own surface speaking, `None` when it says nothing about the
/// road. A `code_cell` result (tool road) or an `[observation]` line (act
/// road) that carries no [`ENV_FAILURE_MARKERS`] means the worker ran; a
/// marker in one means it died. Nothing else is a carrier — in particular a
/// search or shell result that happens to quote a marker is not the road
/// dying.
fn code_signal(m: &ChatMessage) -> Option<bool> {
    let (is_carrier, text): (bool, &str) = match m {
        ChatMessage::Tool { name, content, .. } => {
            (name.as_deref() == Some(CODE_CELL_TOOL), content.as_str())
        }
        ChatMessage::User { content, .. } => {
            (content.starts_with(OBSERVATION_PREFIX), content.as_str())
        }
        _ => return None,
    };
    is_carrier.then(|| {
        !ENV_FAILURE_MARKERS
            .iter()
            .any(|marker| text.contains(marker))
    })
}

/// Whether the code road's last word, in the current user turn, is an env
/// death (see the module docs for the scan's two properties: structured
/// carriers, fresh look per user turn).
pub(crate) fn code_road_dead(messages: &[ChatMessage]) -> bool {
    let mut dead = false;
    for m in messages {
        // A real user turn opens a fresh window (an `[observation]` line is
        // the road speaking, not the user).
        if matches!(m, ChatMessage::User { content, .. } if !content.starts_with(OBSERVATION_PREFIX))
        {
            dead = false;
        }
        if let Some(alive) = code_signal(m) {
            dead = !alive;
        }
    }
    dead
}

/// Per-turn file-op faces (see the module docs).
pub struct FileToolFaceMiddleware {
    /// Whether the code road's engine actually assembled this run (tool or
    /// act). False on the off road and in the kill-switch build.
    code_engine_live: bool,
    /// The `=code` rollback face: code owns file work (see
    /// [`code_owns_file_tools`]).
    code_owns_file_tools: bool,
}

impl FileToolFaceMiddleware {
    pub fn new(code_engine_live: bool, code_owns_file_tools: bool) -> Self {
        Self {
            code_engine_live,
            code_owns_file_tools,
        }
    }
}

#[async_trait]
impl Middleware for FileToolFaceMiddleware {
    async fn on_pre_llm(&self, ctx: &mut PreLlmCtx) -> AgentResult<()> {
        let dead = code_road_dead(&ctx.messages);
        // `=code` face: the triad leaves while the code road owns file work
        // and comes back when it dies.
        if self.code_owns_file_tools && self.code_engine_live && !dead {
            drop_tools(&mut ctx.tools, &FILE_TRIAD);
        }
        // Both faces: a dead code road is no longer a vehicle.
        if dead {
            drop_tools(&mut ctx.tools, &[CODE_CELL_TOOL]);
        }
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

/// What a degraded run tells the user: what was missing and the way out. The
/// file triad is the default vehicle, not a fallback, so the notice says
/// what is missing rather than what "took over".
#[cfg(feature = "code-exec")]
const DEGRADE_NOTICE: &str = "code cells need Python 3 - none found (checked CODEXEC_PYTHON, \
    python3, python). code_cell stays unloaded; read_file/list_files/write_file cover file work \
    as always. Install Python 3 and restart to get code cells";

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
/// no-engine shape (no dead `code_cell` on the menu, no code-cell teaching in
/// the prompt) and say why. The file triad covers file work either way. A
/// configured `off` is the user's own decision and gets no notice.
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

    /// The wire menu both vehicles ride: the triad, `code_cell`, and tools
    /// the face never touches.
    fn menu() -> Vec<Value> {
        FILE_TRIAD
            .iter()
            .map(|n| tool_def(n))
            .chain([
                tool_def(CODE_CELL_TOOL),
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

    fn cell_result(text: &str) -> ChatMessage {
        ChatMessage::Tool {
            tool_call_id: "c1".into(),
            name: Some(CODE_CELL_TOOL.into()),
            content: text.into(),
            images: Vec::new(),
        }
    }

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    // ── Tool filtering ──────────────────────────────────────────────────

    #[test]
    fn dropping_takes_exactly_the_named_tools() {
        let mut tools = menu();
        drop_tools(&mut tools, &FILE_TRIAD);
        assert_eq!(
            names_of(&tools),
            vec![
                CODE_CELL_TOOL,
                "edit_file",
                "search_content",
                "execute_command"
            ],
            "the triad goes, everything else stays"
        );
    }

    #[test]
    fn dropping_the_code_cell_leaves_the_file_tools() {
        let mut tools = menu();
        drop_tools(&mut tools, &[CODE_CELL_TOOL]);
        assert_eq!(
            names_of(&tools),
            vec![
                "read_file",
                "list_files",
                "write_file",
                "edit_file",
                "search_content",
                "execute_command"
            ],
            "the file floor never rides the code cell's cut"
        );
    }

    #[test]
    fn names_outside_the_wire_shape_pass_through() {
        // The filter only reads the OpenAI-style {"function": {"name": …}}
        // shape; anything else is not a managed name and stays.
        let mut tools = vec![
            tool_def("read_file"),
            serde_json::json!({"name": "read_file"}),
        ];
        drop_tools(&mut tools, &FILE_TRIAD);
        assert_eq!(tools.len(), 1, "only the managed shape is filtered");
    }

    // ── Env-failure detection ───────────────────────────────────────────

    #[test]
    fn worker_death_markers_kill_the_road() {
        for marker in [
            "error: worker error: could not spawn python worker: No such file (os error 2)",
            "error: worker error: no worker available",
        ] {
            assert!(
                code_road_dead(&[cell_result(marker)]),
                "a code_cell death result must flip the face: {marker}"
            );
            assert!(
                code_road_dead(&[observation(marker)]),
                "an act-road death observation must flip the face: {marker}"
            );
        }
    }

    #[test]
    fn ordinary_cell_errors_do_not_flip_the_face() {
        assert!(
            !code_road_dead(&[cell_result(
                "error: error in block #0 (ValueError): bad input"
            )]),
            "a cell raising is code failing, not the code road dying"
        );
    }

    #[test]
    fn a_search_hit_on_the_marker_text_cuts_nothing() {
        // Carriers are the code road's own surfaces — this very file's marker
        // strings show up in ripgrep output all the time in dogfooding.
        let hit = ChatMessage::Tool {
            tool_call_id: "c1".into(),
            name: Some("search_content".into()),
            content: "file_face.rs:54: could not spawn python worker".into(),
            images: Vec::new(),
        };
        assert!(
            !code_road_dead(&[hit]),
            "a quoted marker in search output is not the road dying"
        );
    }

    #[test]
    fn a_clean_cell_run_after_a_death_heals_the_road() {
        // Parallel calls: one spawn fails while a sibling's cell runs clean.
        let msgs = vec![
            cell_result("worker error: could not spawn python worker: gone"),
            cell_result("[ok] 3"),
        ];
        assert!(
            !code_road_dead(&msgs),
            "a clean run is the road alive again"
        );
    }

    #[test]
    fn a_new_user_turn_opens_a_fresh_window() {
        // A permanent death costs one failed call per user turn — and resume
        // heals the same way: the user speaks, the stale marker stops mattering.
        let msgs = vec![
            user("do the thing"),
            cell_result("worker error: no worker available"),
            user("and then this"),
        ];
        assert!(
            !code_road_dead(&msgs),
            "the marker before the user turn is last turn's news"
        );
    }

    // ── The per-turn face ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn default_face_keeps_the_triad_and_a_death_drops_the_code_cell() {
        // Clean history: both vehicles ride the menu.
        let mut ctx = ctx_with(vec![user("hello")]);
        FileToolFaceMiddleware::new(true, false)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            names_of(&ctx.tools).len(),
            7,
            "default face: the whole menu rides"
        );

        // After a worker death this turn: the triad is the floor and stays;
        // the dead vehicle goes.
        let mut ctx = ctx_with(vec![
            user("hello"),
            cell_result("error: worker error: could not spawn python worker: gone"),
        ]);
        FileToolFaceMiddleware::new(true, false)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            names_of(&ctx.tools),
            vec![
                "read_file",
                "list_files",
                "write_file",
                "edit_file",
                "search_content",
                "execute_command"
            ],
            "code cell dead: it leaves, the file tools stay"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn code_owns_face_drops_the_triad_until_a_death_brings_it_back() {
        // The =code rollback face: code owns file work while it is alive.
        let mut ctx = ctx_with(vec![user("hello")]);
        FileToolFaceMiddleware::new(true, true)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            names_of(&ctx.tools),
            vec![
                CODE_CELL_TOOL,
                "edit_file",
                "search_content",
                "execute_command"
            ],
            "code alive: the triad is off the wire"
        );

        // Death: the triad is back, and the dead code cell goes with it.
        let mut ctx = ctx_with(vec![
            user("hello"),
            observation("error: worker error: no worker available"),
        ]);
        FileToolFaceMiddleware::new(true, true)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert_eq!(
            names_of(&ctx.tools),
            vec![
                "read_file",
                "list_files",
                "write_file",
                "edit_file",
                "search_content",
                "execute_command"
            ],
            "code dead: the file tools load back and the dead cell leaves"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_engine_keeps_the_file_tools() {
        // Off road / kill-switch build / no session context: no engine
        // behind the code road — the triad stays and there is no code cell
        // to manage (the drop would be a no-op).
        let mut ctx = ctx_with(vec![]);
        FileToolFaceMiddleware::new(false, true)
            .on_pre_llm(&mut ctx)
            .await
            .unwrap();
        assert!(
            names_of(&ctx.tools).contains(&"read_file"),
            "the triad stays even under the =code face without an engine"
        );
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
        // Python missing: the code roads degrade to the no-engine shape and
        // say so; the notice names both escape hatches and does not call the
        // file tools a fallback.
        for mode in [CodeExecMode::Act, CodeExecMode::Tool] {
            let (got, notice) = degrade(mode, false);
            assert_eq!(got, CodeExecMode::Off, "{mode:?} without Python degrades");
            let msg = notice.expect("degrading must be announced");
            assert!(msg.contains("Python 3"), "{msg}");
            assert!(msg.contains("CODEXEC_PYTHON"), "{msg}");
            assert!(msg.contains("read_file/list_files/write_file"), "{msg}");
            assert!(!msg.contains("Fell back"), "{msg}");
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
