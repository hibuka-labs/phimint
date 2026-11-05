//! Browser tool-face loading: per-turn scenario composition + change notice.
//!
//! The 22 CDP browser tools register once (Chrome stays lazy — see
//! `phi_tools::BrowserToolset::lazy`), but the face the model *sees* narrows to
//! the scenario minimum while that scenario is live, and most turns are not
//! browser turns: a coding session that never opens a page pays ~7.5 KB of
//! browser schema **every** request for tools it will never call (税 2, the
//! biggest single region of the menu). So the face resolves per turn to one
//! of three shapes before every LLM call:
//!
//! - **Idle** — no browser work in this session yet: the browser starter set
//!   (the canvas minimum — open the page, look, click, close). Any real
//!   browser task starts there, so nothing is unreachable, and the full
//!   suite arrives the turn after browser work begins.
//! - **Canvas** — a live design canvas: the canvas minimum only (a canvas
//!   self-check never needs more).
//! - **Full** — browser work underway: the whole suite.
//!
//! "Browser work in this session" is derived from the conversation itself
//! (any `browser_*` call in the history — see `history_has_browser_use`),
//! so it survives resume for free and cannot desync from what the session
//! actually did. Residual hole: context rotation can forget *ancient* browser
//! calls while a session is still a browser session — the face then offers
//! the starter set for one turn and returns to Full on the next browser call.
//! That path is self-healing and announced; it never locks a tool out for
//! good.
//!
//! On top of the per-turn face, the composition keeps a `## Browser Tools`
//! system-prompt section in step with the face, so a mid-session change is
//! announced instead of silently shrinking the tool list under the model:
//!
//! 1. drop the browser tool definitions outside the active face (other tools
//!    are untouched), and
//! 2. refresh the `## Browser Tools` section on the outgoing system message.
//!
//! The section is a current-state statement (refreshed in place each turn,
//! the same contract as the skills catalog), not a one-shot event: an LLM
//! call has no memory beyond the messages it is given, so a notice that is
//! not in the prompt of *every* call after the change is a notice the model
//! never sees.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use phi_agent::{AgentResult, ChatMessage, Middleware, PreLlmCtx};
use phi_tools::{BrowserToolGroup, browser_tool_names};
use serde_json::Value;

use crate::tools::canvas::CanvasActivity;

/// Header of the browser tool-face section in the system prompt.
const FACE_HEADER: &str = "## Browser Tools";

/// Byte sequence marking the start of a tool-face region: the `\n\n`
/// separator callers prepend plus the header line.
const FACE_ANCHOR: &str = "\n\n## Browser Tools\n\n";

/// A `## ` section start — the boundary a face region may never cross (same
/// mark the skills catalog surgery uses; both regions can sit in one prompt
/// without seeing each other).
const NEXT_HEADING_MARK: &str = "\n\n## ";

/// Which browser tool face is live this turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FaceMode {
    /// Design canvas open — the canvas minimum.
    Canvas,
    /// No browser work yet this session — the browser starter set (same five
    /// names as [`FaceMode::Canvas`]; only the announcement differs).
    Idle,
    /// Browser work underway — the full suite.
    Full,
}

impl FaceMode {
    /// The face decision: a live design canvas (open and its server up) means
    /// the session is doing canvas work, so only the minimum is exposed;
    /// otherwise browser work in the conversation means the full suite is in
    /// use, and a session that has not touched the browser gets the starter
    /// set.
    pub(crate) fn for_liveness(canvas_live: bool, browser_used: bool) -> Self {
        if canvas_live {
            FaceMode::Canvas
        } else if browser_used {
            FaceMode::Full
        } else {
            FaceMode::Idle
        }
    }

    /// Resolve the face for one LLM call. `pin_full` is the ops rollback seam
    /// ([`face_pinned_full`]) — pinning restores the pre-tool-face behavior
    /// without a rebuild.
    pub(crate) fn resolve(canvas_live: bool, browser_used: bool, pin_full: bool) -> Self {
        if pin_full {
            FaceMode::Full
        } else {
            FaceMode::for_liveness(canvas_live, browser_used)
        }
    }

    /// Tool names visible in this mode.
    pub(crate) fn tool_names(self) -> Vec<&'static str> {
        match self {
            FaceMode::Canvas | FaceMode::Idle => browser_tool_names(&[BrowserToolGroup::Canvas]),
            FaceMode::Full => browser_tool_names(&[BrowserToolGroup::All]),
        }
    }

    /// The system-prompt section announcing this face: `None` for the full
    /// suite (the complete tools array needs no explanation, and the browser
    /// work case pays zero extra tokens), `Some` with the `\n\n` lead-in
    /// included for the two constrained faces.
    pub(crate) fn render_section(self) -> Option<String> {
        match self {
            FaceMode::Full => None,
            FaceMode::Canvas => Some(format!(
                "\n\n{FACE_HEADER}\n\n\
                 The design canvas is open, so only the canvas minimum of browser tools is \
                 registered this turn: `browser_navigate`, `browser_screenshot`, `browser_click`, \
                 `browser_snapshot`, `browser_close`. Other browser automation tools (tabs, forms, \
                 content extraction, history) are not registered while the canvas is open — do not \
                 try to call them. The full browser toolset returns automatically when the canvas \
                 closes.\n"
            )),
            FaceMode::Idle => Some(format!(
                "\n\n{FACE_HEADER}\n\n\
                 No browser work in this session yet, so only the browser starter set is \
                 registered: `browser_navigate`, `browser_screenshot`, `browser_click`, \
                 `browser_snapshot`, `browser_close`. Other browser automation tools (tabs, forms, \
                 content extraction, history) register automatically once browser work starts — \
                 call `browser_navigate` first; the full browser toolset arrives the next turn. \
                 Until then do not try to call the unregistered ones.\n"
            )),
        }
    }
}

/// Locate one tool-face region: from the anchor through the next `## `
/// heading (or end of content).
fn find_face_region(content: &str) -> Option<(usize, usize)> {
    let start = content.find(FACE_ANCHOR)?;
    let search_from = start + FACE_ANCHOR.len();
    let end = content[search_from..]
        .find(NEXT_HEADING_MARK)
        .map_or(content.len(), |rel| search_from + rel);
    Some((start, end))
}

/// Swap the `## Browser Tools` region for `section` (or drop it when `None`).
///
/// Steady state — one region, same section text — is a fixed point, so an
/// unchanged face keeps the prompt byte-identical for provider-side caching.
/// Content outside the region is preserved byte-for-byte.
pub(crate) fn refresh_face_section(content: &str, section: Option<&str>) -> String {
    match (find_face_region(content), section) {
        (Some((start, end)), Some(new)) => {
            format!("{}{}{}", &content[..start], new, &content[end..])
        }
        (Some((start, end)), None) => format!("{}{}", &content[..start], &content[end..]),
        (None, Some(new)) => format!("{content}{new}"),
        (None, None) => content.to_string(),
    }
}

/// Drop browser tool definitions outside the active face.
///
/// Only the registered suite is managed — anything else (including a future
/// `browser_*` tool from another source) is left untouched.
pub(crate) fn filter_browser_tools(tools: &mut Vec<Value>, mode: FaceMode) {
    let keep: HashSet<&str> = mode.tool_names().into_iter().collect();
    let suite: HashSet<&str> = BrowserToolGroup::All.tool_names().into_iter().collect();
    tools.retain(|def| {
        let name = def
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str());
        match name {
            Some(n) if suite.contains(n) => keep.contains(n),
            _ => true,
        }
    });
}

/// Whether this session has done browser work: any `browser_*` call in the
/// conversation (the assistant's tool calls, or a tool result naming its
/// tool). Derived from the messages themselves — no hidden state to desync,
/// and a resumed session inherits its answer from the history it restores.
pub(crate) fn history_has_browser_use(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|m| match m {
        ChatMessage::Assistant { tool_calls, .. } => tool_calls
            .as_ref()
            .is_some_and(|calls| calls.iter().any(|c| c.name.starts_with("browser_"))),
        ChatMessage::Tool { name, .. } => {
            name.as_deref().is_some_and(|n| n.starts_with("browser_"))
        }
        _ => false,
    })
}

/// Per-turn browser tool-face composition (see the module docs).
pub struct BrowserToolFaceMiddleware {
    activity: Arc<CanvasActivity>,
}

impl BrowserToolFaceMiddleware {
    pub fn new(activity: Arc<CanvasActivity>) -> Self {
        Self { activity }
    }
}

/// Ops rollback seam: `PHIMINT_BROWSER_TOOL_FACE=full` pins the full suite
/// regardless of canvas state — the behavior before tool-face loading — so a
/// bad grouping can be turned off in the field without a rebuild.
fn face_pinned_full() -> bool {
    std::env::var_os("PHIMINT_BROWSER_TOOL_FACE").is_some_and(|v| v == "full")
}

#[async_trait]
impl Middleware for BrowserToolFaceMiddleware {
    async fn on_pre_llm(&self, ctx: &mut PreLlmCtx) -> AgentResult<()> {
        let canvas_live = self.activity.is_canvas_live().await;
        let browser_used = history_has_browser_use(&ctx.messages);
        let mode = FaceMode::resolve(canvas_live, browser_used, face_pinned_full());
        filter_browser_tools(&mut ctx.tools, mode);
        let section = mode.render_section();
        match ctx.messages.first_mut() {
            Some(ChatMessage::System { content, .. }) => {
                *content = refresh_face_section(content, section.as_deref());
            }
            // Skipping silently would hide a wiring error where the face
            // narrows but the model is never told — at least leave a log.
            _ => tracing::warn!(
                "browser tool-face notice skipped: first message is not System \
                 (the tool face is filtered but the change is not announced)"
            ),
        }
        Ok(())
    }
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

    fn full_tool_face() -> Vec<Value> {
        BrowserToolGroup::All
            .tool_names()
            .into_iter()
            .map(tool_def)
            .chain([tool_def("read_file"), tool_def("execute_command")])
            .collect()
    }

    fn names_of(tools: &[Value]) -> Vec<&str> {
        tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect()
    }

    fn make_pre_llm_ctx(system_content: &str) -> PreLlmCtx {
        PreLlmCtx {
            session_id: SessionId {
                id: 1,
                external_id: None,
            },
            messages: vec![ChatMessage::System {
                content: system_content.to_string(),
                ephemeral: false,
            }],
            tools: full_tool_face(),
            emit_fn: None,
            turn_count: 1,
            max_turns: 10,
        }
    }

    // ── Tool filtering ──────────────────────────────────────────────────

    #[test]
    fn pin_full_is_the_rollback_seam() {
        // Pinning forces the full suite even over a live canvas or an idle
        // session; unpinned follows liveness as usual.
        assert_eq!(
            FaceMode::resolve(true, true, true),
            FaceMode::Full,
            "rollback seam overrides canvas liveness"
        );
        assert_eq!(FaceMode::resolve(false, false, true), FaceMode::Full);
        assert_eq!(FaceMode::resolve(true, true, false), FaceMode::Canvas);
        assert_eq!(FaceMode::resolve(true, false, false), FaceMode::Canvas);
        assert_eq!(FaceMode::resolve(false, true, false), FaceMode::Full);
        assert_eq!(FaceMode::resolve(false, false, false), FaceMode::Idle);
    }

    #[test]
    fn idle_face_keeps_the_starter_five() {
        // The starter set is the canvas minimum: enough to open the page,
        // look, click, close — any browser task can start here, and the rest
        // arrives once browser work begins.
        let mut tools = full_tool_face();
        filter_browser_tools(&mut tools, FaceMode::Idle);
        let names = names_of(&tools);
        assert_eq!(
            names,
            vec![
                "browser_navigate",
                "browser_click",
                "browser_screenshot",
                "browser_snapshot",
                "browser_close",
                "read_file",
                "execute_command",
            ],
            "starter set stays, non-browser tools are untouched"
        );
    }

    #[test]
    fn only_browser_calls_mark_browser_work() {
        let other_work = vec![
            ChatMessage::Assistant {
                content: Some("hi".into()),
                reasoning_content: None,
                thinking_signature: None,
                tool_calls: Some(vec![phi_agent::llm_trait::ToolCallMessage {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }]),
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                name: Some("read_file".into()),
                content: "x".into(),
                images: Vec::new(),
            },
            ChatMessage::User {
                content: "q".into(),
                images: vec![],
                ephemeral: false,
                synthetic: false,
            },
        ];
        assert!(!history_has_browser_use(&other_work), "no browser names");
        assert!(history_has_browser_use(&browser_call_history()));
    }

    #[test]
    fn canvas_face_keeps_exactly_the_five() {
        let mut tools = full_tool_face();
        filter_browser_tools(&mut tools, FaceMode::Canvas);
        let names = names_of(&tools);
        assert_eq!(
            names,
            vec![
                "browser_navigate",
                "browser_click",
                "browser_screenshot",
                "browser_snapshot",
                "browser_close",
                "read_file",
                "execute_command",
            ],
            "canvas minimum stays, non-browser tools are untouched"
        );
    }

    #[test]
    fn full_face_keeps_everything() {
        let mut tools = full_tool_face();
        let before: Vec<String> = names_of(&tools).iter().map(|s| s.to_string()).collect();
        filter_browser_tools(&mut tools, FaceMode::Full);
        let after: Vec<String> = names_of(&tools).iter().map(|s| s.to_string()).collect();
        assert_eq!(after, before);
    }

    #[test]
    fn unmanaged_names_pass_through() {
        let mut tools = vec![tool_def("browser_future_tool"), tool_def("mcp_thing")];
        filter_browser_tools(&mut tools, FaceMode::Canvas);
        assert_eq!(names_of(&tools), vec!["browser_future_tool", "mcp_thing"]);
    }

    // ── Section surgery ─────────────────────────────────────────────────

    #[test]
    fn section_is_idempotent_in_place() {
        let base = "You are phimint.\n\n## Skills\n\n- alpha: x\n";
        let section = FaceMode::Canvas.render_section().unwrap();
        let once = refresh_face_section(base, Some(&section));
        assert!(once.contains(FACE_HEADER));
        assert!(once.starts_with(base.trim_end_matches('\n')), "prefix kept");
        let twice = refresh_face_section(&once, Some(&section));
        assert_eq!(once, twice, "refresh must be a fixed point");
    }

    #[test]
    fn existing_section_sits_between_headings_and_swap_keeps_neighbors() {
        // Swap path (the region already in the prompt): neighbors on both
        // sides keep their bytes — same contract as the skills catalog, so a
        // host suffix can follow the face section without being eaten.
        let lead = "You are phimint.\n\n## Skills\n\n- alpha: x\n";
        let suffix = "\n\n## Context Management\n\nuse history tools.";
        let section = FaceMode::Canvas.render_section().unwrap();
        let composed = format!("{lead}{section}{suffix}");
        assert_eq!(
            refresh_face_section(&composed, Some(&section)),
            composed,
            "swap with the same text must be a fixed point"
        );

        // Face returns to full → the note goes away and the prompt comes
        // back byte-identical to the note-free composition.
        let cleared = refresh_face_section(&composed, None);
        assert_eq!(cleared, format!("{lead}{suffix}"));
    }

    #[test]
    fn section_without_a_region_appends_at_the_end() {
        // The production shape: the builder composes no face section (build
        // time cannot know the face), so the first constrained turn appends
        // one to the outgoing system message.
        let base = "You are phimint.\n\n## Skills\n\n- alpha: x\n";
        let section = FaceMode::Canvas.render_section().unwrap();
        let appended = refresh_face_section(base, Some(&section));
        assert!(appended.starts_with(base), "base prompt preserved");
        assert!(appended.ends_with(section.trim_start_matches('\n')));
        assert_eq!(appended.matches(FACE_HEADER).count(), 1);
    }

    #[test]
    fn full_mode_renders_no_section() {
        assert!(FaceMode::Full.render_section().is_none());
        let base = "prompt";
        assert_eq!(refresh_face_section(base, None), base);
    }

    #[test]
    fn face_change_is_visible_to_the_next_call() {
        // What the middleware actually does per turn: the section is swapped
        // in place on the outgoing system message, so the turn after a
        // mid-session canvas open sees the constrained face announced.
        let mut ctx = make_pre_llm_ctx("You are phimint.");
        let section = FaceMode::Canvas.render_section().unwrap();
        if let Some(ChatMessage::System { content, .. }) = ctx.messages.first_mut() {
            *content = refresh_face_section(content, Some(&section));
        }
        match &ctx.messages[0] {
            ChatMessage::System { content, .. } => {
                assert!(content.contains("browser_navigate"));
                assert!(content.contains("only the canvas minimum"));
            }
            other => panic!("expected System, got {other:?}"),
        }
    }

    // ── End-to-end middleware (liveness → face + notice) ────────────────

    /// Drive one `on_pre_llm` the way the react loop does and hand back what
    /// the LLM would see: the filtered tools and the system message. `history`
    /// rides after the system message (restored turns, as resume provides).
    async fn run_middleware_with(
        activity: &Arc<CanvasActivity>,
        history: Vec<ChatMessage>,
    ) -> (Vec<Value>, String) {
        let mw = BrowserToolFaceMiddleware::new(activity.clone());
        let mut ctx = make_pre_llm_ctx("You are phimint.");
        ctx.messages.extend(history);
        mw.on_pre_llm(&mut ctx).await.unwrap();
        let system = match &ctx.messages[0] {
            ChatMessage::System { content, .. } => content.clone(),
            other => panic!("expected System, got {other:?}"),
        };
        (ctx.tools, system)
    }

    async fn run_middleware(activity: &Arc<CanvasActivity>) -> (Vec<Value>, String) {
        run_middleware_with(activity, vec![]).await
    }

    fn browser_call_history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::Assistant {
                content: None,
                reasoning_content: None,
                thinking_signature: None,
                tool_calls: Some(vec![phi_agent::llm_trait::ToolCallMessage {
                    id: "c1".into(),
                    name: "browser_navigate".into(),
                    arguments: "{\"url\":\"https://example.com\"}".into(),
                }]),
            },
            ChatMessage::Tool {
                tool_call_id: "c1".into(),
                name: Some("browser_navigate".into()),
                content: "opened".into(),
                images: Vec::new(),
            },
        ]
    }

    fn browser_count(defs: &[Value]) -> usize {
        defs.iter()
            .filter(|d| {
                d["function"]["name"]
                    .as_str()
                    .unwrap()
                    .starts_with("browser_")
            })
            .count()
    }

    #[tokio::test]
    async fn no_canvas_no_browser_history_means_the_idle_starter_face() {
        // A fresh coding session (the common case) pays for the starter set
        // only — 税 2's biggest region stays off the wire until it is needed.
        let activity = Arc::new(CanvasActivity::new());
        let (tools, system) = run_middleware(&activity).await;
        assert_eq!(browser_count(&tools), 5, "starter five only");
        assert_eq!(tools.len(), 7, "5 browser tools + 2 others");
        assert!(system.contains(FACE_HEADER), "the narrowing is announced");
        assert!(
            system.contains("the full browser toolset arrives the next turn"),
            "the growth path is named, not left to surprise: {system}"
        );
    }

    #[tokio::test]
    async fn browser_work_in_the_history_keeps_the_full_face() {
        // The liveness signal is the conversation itself: once browser work
        // appears (assistant tool call or tool result naming the tool), the
        // full suite rides every turn and no note is needed.
        let activity = Arc::new(CanvasActivity::new());
        let (tools, system) = run_middleware_with(&activity, browser_call_history()).await;
        assert_eq!(browser_count(&tools), 22, "full suite after browser work");
        assert!(!system.contains(FACE_HEADER), "full face needs no note");

        // The tool-result arm alone flips it too (history shapes vary).
        let tool_only = vec![ChatMessage::Tool {
            tool_call_id: "c9".into(),
            name: Some("browser_screenshot".into()),
            content: "img".into(),
            images: Vec::new(),
        }];
        let (tools, _) = run_middleware_with(&activity, tool_only).await;
        assert_eq!(browser_count(&tools), 22, "tool result names the tool");
    }

    #[tokio::test]
    async fn live_canvas_narrows_the_face_and_announces_it() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(phi_canvas::CanvasTools::new(
            tmp.path().join("canvas").join("sess-1"),
            "sess-1",
        ));
        // No launcher: the tool binds the server and reports "awaiting
        // consent" — the canvas is live either way, which is all the face
        // decision reads.
        let text = tools.open().await.unwrap();
        assert!(text.starts_with("canvas:"), "canvas bound: {text}");

        let activity = Arc::new(CanvasActivity::new());
        activity.bind(tools.clone()).await;
        assert!(activity.is_canvas_live().await);

        let (defs, system) = run_middleware(&activity).await;
        let names = names_of(&defs);
        assert_eq!(
            names.iter().filter(|n| n.starts_with("browser_")).count(),
            5,
            "only the canvas minimum is exposed: {names:?}"
        );
        assert!(names.contains(&"read_file"), "other tools untouched");
        assert!(system.contains(FACE_HEADER), "the change is announced");
        assert!(system.contains("`browser_screenshot`"));

        // Canvas closes mid-self-check history → the full face returns and
        // the note goes away (the canvas calls themselves are the browser
        // work that reopens the suite).
        tools.close().await.unwrap();
        assert!(!activity.is_canvas_live().await);
        let (defs, system) = run_middleware_with(&activity, browser_call_history()).await;
        assert_eq!(browser_count(&defs), 22);
        assert!(!system.contains(FACE_HEADER));
    }

    #[tokio::test]
    async fn unbind_forgets_the_session_canvas() {
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(phi_canvas::CanvasTools::new(
            tmp.path().join("canvas").join("sess-2"),
            "sess-2",
        ));
        tools.open().await.unwrap();
        let activity = CanvasActivity::new();
        activity.bind(tools).await;
        assert!(activity.is_canvas_live().await);
        activity.unbind().await;
        assert!(!activity.is_canvas_live().await);
    }
}
