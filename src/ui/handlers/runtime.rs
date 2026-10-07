//! Runtime event handler for the TUI application.
//!
//! Handles `RuntimeEvent` from the agent, updating the application state
//! accordingly. This includes:
//! - Text streaming (TextDelta, ThoughtDelta)
//! - Tool calls (ToolCallStarted, ToolCallFinished)
//! - Plan updates (PlanUpdated)
//! - Approval requests (AwaitingApproval)
//! - Run lifecycle (RunFinished, RunCancelled)
//! - User events (Progress, Notice)

use phi_agent::RuntimeEvent;
use phi_agent::{NoticeKind, PlanStepStatus, UserEvent};
use std::time::Instant;

use crate::banner::BannerStyle;
use crate::ui::app::{AgentStatus, App, Phase, SubAgentStatus, ToolEvent};
use phi_tui::diff::{diff_to_hunks, diff_to_hunks_indexed};
use phi_tui::lines::{DiffHunk, LineDetail, LineKind, MetaHead, OutputLine, ToolState};
use phi_tui::wrap::{one_line, wrap};

/// Render a notice line for the TUI.
///
/// Product-layer copy table (design §8): the mechanism layer emits
/// cause-specific English facts ("guard judge unparsed/call failed/unavailable
/// — treating as complete"), but end users have never heard of a "judge" and
/// cannot act on the cause — the three collapse to one user-facing line. The
/// cause stays in `session.log` for debugging. Everything else renders as
/// `source: text`, unless `text` already names the source (no doubling).
fn notice_line(source: &str, text: &str) -> String {
    if source == "guard" && text.starts_with("guard judge") {
        return "Answer accepted without verification.".to_string();
    }
    if text.starts_with(source) {
        text.to_string()
    } else {
        format!("{source}: {text}")
    }
}

/// Build the structured block a tool attaches under its invocation line, or
/// `None` when the call has no payload worth a block.
///
/// Three shapes, and the split is deliberate (design §write):
/// - `edit_file` → [`LineDetail::Diff`] — an edit is a *transformation*, and
///   its evidence is never previewed away. Rendered in full, outside the
///   preview ladder.
/// - `write_file` → [`LineDetail::Folded`] holding the content being written.
///   A create is an *artifact*, not a transformation: the useful facts are
///   "what shape, how big", so it rides the preview ladder. If the call turns
///   out to be an overwrite, [`App::handle_runtime`] swaps this for a
///   real old→new diff — an overwrite *is* a transformation.
/// - `spawn_agent` → [`LineDetail::Folded`] holding the task brief. Also an
///   artifact (a briefing, not a change): the row keeps a gist of the head,
///   the brief rides the ladder.
fn build_tool_detail(tool_name: &str, args_json: &str) -> Option<LineDetail> {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(tool = tool_name, error = %e, "build_tool_detail: failed to parse args_json");
            return None;
        }
    };
    // `spawn_agent` → the task brief. An artifact like written content: what
    // matters at a glance is "who, roughly what", so the invocation row keeps
    // the gist and the brief rides the preview ladder, whole.
    if tool_name == "spawn_agent" {
        let task = args.get("task").and_then(|v| v.as_str())?;
        if task.is_empty() {
            return None;
        }
        let (_, meta_head) = spawn_task_head(task);
        return Some(LineDetail::Folded {
            raw: task.to_string(),
            line_count: task.lines().count(),
            char_count: task.chars().count(),
            meta_head,
        });
    }
    let path = {
        let p = args.get("path").and_then(|v| v.as_str())?;
        p.to_string()
    };

    match tool_name {
        "edit_file" => {
            let edits = args.get("edits")?.as_array()?;
            let mut all_hunks: Vec<DiffHunk> = Vec::new();
            for (i, edit) in edits.iter().enumerate() {
                let old_text = edit.get("old_text")?.as_str()?;
                let new_text = edit.get("new_text")?.as_str()?;
                all_hunks.extend(diff_to_hunks_indexed(old_text, new_text, 3, i));
            }
            if all_hunks.is_empty() {
                return None;
            }
            Some(LineDetail::Diff {
                path,
                hunks: all_hunks,
            })
        }
        "write_file" => {
            let content = args.get("content").and_then(|v| v.as_str())?;
            if content.is_empty() {
                return None;
            }
            Some(LineDetail::Folded {
                raw: content.to_string(),
                line_count: content.lines().count(),
                char_count: content.chars().count(),
                // The meta row is the invocation (`* write_file <path>`); it
                // says nothing about the content, so the body owns every row.
                meta_head: MetaHead::None,
            })
        }
        _ => None,
    }
}

/// Basename of a filesystem path for an invocation line.
///
/// The file name is what identifies the call; the parent directories are the
/// same `/Users/…/projects/-Users-…/memory/` prefix repeated on every row.
/// The full path is still in `session.log` and in the tool result.
fn path_label(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Argument values that identify a call, with the JSON syntax dropped.
///
/// `args_json` is a syntax dump — `{"limit": 60, "path": "/Users/…/x.md"}` —
/// whose braces, quoted keys and full paths eat the line budget while adding
/// nothing the tool name doesn't already say. The transcript shows the values
/// (rule 3: 工具名 + 可读参数); the raw JSON stays in the log.
///
/// Content payloads (`content`, `old_text`/`new_text`, `edits`, `task`) are
/// never inlined here — they are either multi-KB or already rendered as the
/// tool's structured block.
fn args_label(tool_name: &str, args: &serde_json::Value) -> String {
    // Payload keys shown elsewhere (structured block) or too long to inline.
    const SKIP: &[&str] = &[
        "content",
        "old_text",
        "new_text",
        "edits",
        "task",
        "description",
        "prompt",
    ];
    // Path-shaped keys: show the basename only.
    const PATHISH: &[&str] = &["path", "file", "file_path", "dir", "directory", "target"];

    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };

    // read_file's identifying detail is the range, not just the name.
    if tool_name == "read_file" {
        let mut out = match obj.get("path").and_then(|v| v.as_str()) {
            Some(p) => path_label(p),
            None => String::new(),
        };
        if let Some(off) = obj.get("offset").and_then(|v| v.as_u64()) {
            out.push_str(&format!(" from L{}", off + 1));
        }
        return out;
    }

    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj {
        if SKIP.contains(&k.as_str()) {
            continue;
        }
        if PATHISH.contains(&k.as_str()) {
            if let Some(p) = v.as_str() {
                let label = path_label(p);
                if !label.is_empty() {
                    parts.push(label);
                }
            }
            continue;
        }
        match v {
            serde_json::Value::String(s) if s.is_empty() => {}
            serde_json::Value::String(s) => {
                // A command or a pattern is worth showing whole-ish; a bare name
                // reads better without its key.
                let shown = one_line(s, 60);
                if k == "name" || k == "pattern" {
                    parts.push(shown);
                } else {
                    parts.push(format!("{k}={shown}"));
                }
            }
            serde_json::Value::Number(n) => parts.push(format!("{k}={n}")),
            serde_json::Value::Bool(b) => parts.push(format!("{k}={b}")),
            serde_json::Value::Null => {}
            // Objects/arrays are structure, not values — they live in the
            // structured block or the log.
            _ => {}
        }
    }
    parts.join("  ")
}

/// Invocation-line body for a tool call: `tool_name` plus readable arguments.
/// No leading glyph — the renderer draws that from the call's [`ToolState`].
fn tool_invocation_text(tool_name: &str, args_json: &str, prefix: &str) -> String {
    if tool_name == "spawn_agent" {
        return spawn_invocation_text(args_json, prefix)
            .unwrap_or_else(|| format!("{prefix}{tool_name}"));
    }
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return format!("{prefix}{tool_name}"),
    };
    let label = args_label(tool_name, &args);
    if label.is_empty() {
        format!("{prefix}{tool_name}")
    } else {
        format!("{prefix}{tool_name}  {label}")
    }
}

/// Budget for the task gist on a `spawn_agent` invocation line, and for the
/// outcome message on its result line.
///
/// **Not** the terminal width: the gist is a label, and a label that
/// re-expands into a paragraph on a wide terminal is the exact dump this line
/// exists to avoid (session 20261006_9264ba3e — four spawn briefs ate the
/// pane). The task brief itself rides the preview ladder, whole, one
/// keypress away.
const SPAWN_GIST_COLS: usize = 56;

/// The task head a spawn invocation line shows, and how it relates to the
/// payload's first line (`MetaHead`), for the fold that carries the brief.
fn spawn_task_head(task: &str) -> (String, MetaHead) {
    let head = task.lines().next().unwrap_or("").trim();
    let gist = one_line(head, SPAWN_GIST_COLS);
    let meta_head = if gist == head {
        MetaHead::Whole
    } else {
        MetaHead::Abbreviated
    };
    (gist, meta_head)
}

/// Compact invocation text for `spawn_agent`: `spawn_agent <name> - <task
/// gist>`. The raw args JSON is a multi-KB dump (the full self-contained
/// task text) — in session 20260904_e6612477 its invisible tail was the dead
/// air of a 20 s LLM call. Name + one task line is all the transcript needs;
/// the fold under the line, the task panel and the tool result carry the rest.
/// `None` → the caller falls back to the generic rendering.
fn spawn_invocation_text(args_json: &str, prefix: &str) -> Option<String> {
    let args: serde_json::Value = serde_json::from_str(args_json).ok()?;
    let name = args.get("task_name").and_then(|v| v.as_str())?;
    let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
    let (gist, _) = spawn_task_head(task);
    Some(if gist.is_empty() {
        format!("{prefix}spawn_agent {name}")
    } else {
        format!("{prefix}spawn_agent {name} - {gist}")
    })
}

/// Human label for a `spawn_agent` result.
///
/// The tool answers with structured JSON (`{"agent_path":…,"message":…}`),
/// and a raw JSON row is a machine dump where the transcript wants a fact:
/// whom was spawned, with what capability. The two facts that must never be
/// silent — a recycled registration slot, a read-only degradation — come
/// from the tool's own `message`; the registered-tool list is a dump and
/// rides the fold with the rest of the raw result.
///
/// `None` when the summary is not that shape → the generic head/meta path.
fn spawn_result_label(summary: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(summary).ok()?;
    let path = v.get("agent_path").and_then(serde_json::Value::as_str)?;
    let msg = v
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if path.is_empty() {
        // The spawn failed and the orphan was closed — the message *is* the
        // outcome ("Agent spawned but task delivery failed…").
        return Some(one_line(msg, SPAWN_GIST_COLS));
    }
    let mut facts: Vec<&str> = Vec::new();
    if msg.contains("recycled a finished agent") {
        facts.push("recycled");
    }
    if msg.contains("tools degraded to read-only") {
        facts.push("degraded to read-only");
    } else if let Some(cap) = msg
        .split_once("(tools: ")
        .and_then(|(_, rest)| rest.split([';', ')']).next())
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        facts.push(cap);
    }
    if facts.is_empty() {
        Some(format!("spawned {path}"))
    } else {
        Some(format!("spawned {path} ({})", facts.join(", ")))
    }
}

/// Prefix for sub-agent output lines (e.g., `[task_name] `).
pub(crate) fn agent_prefix(agent_id: Option<&str>) -> String {
    match agent_id {
        Some(id) if !id.is_empty() => format!("[{id}] "),
        _ => String::new(),
    }
}

/// Settle the last still-running tool invocation in `output`: refill its state,
/// and for a `write_file` overwrite swap the staged written-content block for a
/// real old→new diff (an overwrite is a transformation, so it earns the same
/// always-full diff treatment an edit gets).
///
/// Returns whether that invocation is the transcript's last line — i.e. the
/// result about to be appended sits directly under its own call and can drop
/// the redundant tool name.
fn settle_tool_call(
    output: &mut [OutputLine<BannerStyle>],
    settled: ToolState,
    details: Option<&serde_json::Value>,
) -> bool {
    let inv_idx = output
        .iter()
        .rposition(|l| l.kind == LineKind::Tool && l.tool_state == Some(ToolState::Running));
    let Some(idx) = inv_idx else {
        return false;
    };
    let adjacent = idx + 1 == output.len();
    let inv = &mut output[idx];
    inv.tool_state = Some(settled);

    // Overwrite is a transformation, so it earns a diff. The tool reports
    // `write_mode` + `old_content`; the new content is what we staged on the
    // line at `ToolCallStarted`.
    let Some(det) = details else { return adjacent };
    if det.get("write_mode").and_then(|v| v.as_str()) != Some("overwrite") {
        return adjacent;
    }
    let Some(LineDetail::Folded {
        raw: new_content, ..
    }) = inv.detail.clone()
    else {
        return adjacent;
    };
    let old = det
        .get("old_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = det
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default();
    let hunks = diff_to_hunks(old, &new_content, 3);
    if !hunks.is_empty() {
        inv.detail = Some(LineDetail::Diff { path, hunks });
    }
    adjacent
}

impl App {
    /// Reflect streaming activity in the status strip — root events only.
    /// Child events arrive through the persistent bus subscription even while
    /// the root is Waiting/Idle; they must not flip the root's state back to
    /// Running.
    fn note_activity(&mut self, agent_id: Option<&str>, phase: Phase) {
        if agent_id.is_none_or(|id| id.is_empty()) {
            // Start the stretch's clock on the first busy signal — not only on
            // Idle → Running: the user-send path and an approval grant set the
            // status directly, and a stretch that never started its clock
            // renders a frozen spinner with no elapsed suffix (the perceived
            // hang of session 20261006_9264ba3e).
            self.begin_activity();
            self.status = AgentStatus::Running { phase };
        }
    }

    /// Handle a runtime event from the agent.
    pub fn handle_runtime(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TextDelta { text, agent_id, .. } => {
                self.track_agent(agent_id.as_deref());
                let child = agent_id.as_deref().filter(|id| !id.is_empty());
                match child {
                    // Child deltas accumulate per-child (never in the shared
                    // `stream`: its pending tail IS the main view's live tail,
                    // and a minutes-long child run flooded it — session
                    // 20260904_3eeb5610). Flushed lines land in the child's
                    // own transcript; visibility = task panel, not the main view.
                    Some(id) => {
                        let had_thought = self
                            .child_streams
                            .get(id)
                            .is_some_and(|s| s.has_pending_thought());
                        let flushed = self.push_child_text(id, &text);
                        self.sub_agent_transcripts
                            .entry(id.to_string())
                            .or_default()
                            .extend(flushed);
                        // push_text implicitly flushes a pending thought first
                        // (StreamState-internal, invisible to flush_pending) —
                        // retire this stream's panel timer so the elapsed
                        // readout doesn't outlive its segment.
                        if had_thought {
                            self.thinking_since.remove(id);
                        }
                    }
                    None => {
                        let had_thought = self.stream.has_pending_thought();
                        let flushed = self.stream.push_text(&text, agent_id.as_deref());
                        for line in flushed {
                            self.transcript.push(line);
                        }
                        // push_text implicitly flushes a pending thought first
                        // (StreamState-internal, invisible to flush_pending) —
                        // retire this stream's panel timer so the elapsed
                        // readout doesn't outlive its segment.
                        if had_thought {
                            self.thinking_since.remove("");
                        }
                    }
                }
                self.note_activity(agent_id.as_deref(), Phase::Streaming);
            }
            RuntimeEvent::ThoughtDelta { text, agent_id, .. } => {
                self.track_agent(agent_id.as_deref());
                let child = agent_id.as_deref().filter(|id| !id.is_empty());
                match child {
                    Some(id) => {
                        let was = self
                            .child_streams
                            .get(id)
                            .is_some_and(|s| s.has_pending_thought());
                        let flushed = self.push_child_thought(id, &text);
                        // push_thought implicitly flushes the previous segment on a
                        // kind/agent flip — that starts a NEW segment, so re-arm the
                        // timer even when `was`, or elapsed overstates the new
                        // segment's age.
                        let flushed_thought = flushed
                            .iter()
                            .any(|l| l.kind == phi_tui::lines::LineKind::Thought);
                        self.sub_agent_transcripts
                            .entry(id.to_string())
                            .or_default()
                            .extend(flushed);
                        if !was || flushed_thought {
                            self.thinking_since.insert(id.to_string(), Instant::now());
                        }
                    }
                    None => {
                        let was = self.stream.has_pending_thought();
                        let flushed = self.stream.push_thought(&text, agent_id.as_deref());
                        // Same implicit-flush re-arm as the child arm (e.g. a root
                        // thought arriving under a different `agent_id` tag).
                        let flushed_thought = flushed
                            .iter()
                            .any(|l| l.kind == phi_tui::lines::LineKind::Thought);
                        for line in flushed {
                            self.transcript.push(line);
                        }
                        if !was || flushed_thought {
                            self.thinking_since.insert(String::new(), Instant::now());
                        }
                    }
                }
                self.note_activity(agent_id.as_deref(), Phase::Thinking);
            }
            RuntimeEvent::ToolCallStarted {
                tool_name,
                args_json,
                agent_id,
                ..
            } => {
                self.flush_pending();
                // Child tool call: commit that child's pending stream first so
                // its text lands above the invocation line in its transcript.
                if let Some(id) = agent_id.as_deref().filter(|id| !id.is_empty()) {
                    let flushed = self.flush_child_stream(id);
                    self.sub_agent_transcripts
                        .entry(id.to_string())
                        .or_default()
                        .extend(flushed);
                }
                // Fresh tool call → drop any progress from the previous one.
                self.live_progress = None;
                // Root calls materialize together, after the stream drains —
                // the drafting window is over. Child calls must not clear the
                // root's drafts.
                if agent_id.as_deref().filter(|id| !id.is_empty()).is_none() {
                    self.tool_drafts.clear();
                }
                // Tools render inline in the transcript (Claude Code style): an
                // invocation line now, a result line on `ToolCallFinished`.
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());
                // Update SubAgentState events
                if let Some(id) = agent_id.as_deref()
                    && let Some(state) = self.sub_agents.get_mut(id)
                {
                    state.last_tool_at = Instant::now();
                    state.events.push(ToolEvent {
                        tool_name: tool_name.clone(),
                        summary: String::new(),
                        is_finished: false,
                    });
                }
                // `update_plan` renders as a plan block (see `PlanUpdated`), so both
                // its invocation line (raw JSON args) and result line are noise —
                // suppress them. Its status transition is still applied.
                if tool_name != "update_plan" {
                    // For payload tools, attach the structured block (edit diff /
                    // written content / spawn brief) the ladder expands below
                    // this line.
                    let detail = build_tool_detail(&tool_name, &args_json);
                    let body = tool_invocation_text(&tool_name, &args_json, &prefix);
                    // The `*` is a marker slot, not a glyph: the renderer draws
                    // the state glyph over it (braille while running, `o`/`x`
                    // when settled). Keeping a stable placeholder means the
                    // line's plain text is self-describing in the log.
                    let text = format!("* {body}");
                    let line = OutputLine {
                        spans: None,
                        original: None,
                        detail,
                        text,
                        kind: LineKind::Tool,
                        tool_state: Some(ToolState::Running),
                    };
                    // Route to the correct transcript
                    if let Some(id) = agent_id.as_deref() {
                        if !id.is_empty() {
                            self.sub_agent_transcripts
                                .entry(id.to_string())
                                .or_default()
                                .push(line);
                        } else {
                            self.transcript.push(line);
                        }
                    } else {
                        self.transcript.push(line);
                    }
                }
                self.note_activity(agent_id.as_deref(), Phase::ToolCall { tool: tool_name });
            }
            RuntimeEvent::ToolCallFinished {
                tool_name,
                summary,
                denied,
                agent_id,
                details,
                ..
            } => {
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());

                // Update SubAgentState events
                if let Some(id) = agent_id.as_deref()
                    && let Some(state) = self.sub_agents.get_mut(id)
                {
                    // Find the last unfinished event and mark it as finished
                    if let Some(event) = state.events.iter_mut().rev().find(|e| !e.is_finished) {
                        event.summary = summary.clone();
                        event.is_finished = true;
                    }
                    state.last_tool_at = Instant::now();
                }

                // Fill the invocation line's state in place (the marker slot is
                // redrawn from this) and, for a `write_file` overwrite, swap the
                // written-content block for a real old→new diff.
                let settled = if denied {
                    ToolState::Denied
                } else {
                    ToolState::Done
                };
                let is_child = matches!(agent_id.as_deref(), Some(id) if !id.is_empty());
                let adjacent = if is_child {
                    let id = agent_id.as_deref().unwrap_or_default().to_string();
                    let output = self.sub_agent_transcripts.entry(id).or_default();
                    settle_tool_call(output, settled, details.as_ref())
                } else {
                    settle_tool_call(&mut self.transcript.output, settled, details.as_ref())
                };

                // `update_plan` renders as a plan block (see `PlanUpdated`), so its
                // tool-result line is redundant — suppress it. Everything else
                // still finalizes the tool-progress state.
                if tool_name != "update_plan" {
                    let (text, kind, detail) = if denied {
                        (
                            format!("  {prefix}⛔ {tool_name} denied"),
                            LineKind::Error,
                            None,
                        )
                    } else if let Some(label) = spawn_result_label(&summary) {
                        // A spawn answers with structured JSON. The meta row
                        // states the fact the row exists for (whom, with what
                        // capability) and the raw result rides the fold — the
                        // registered-tool dump and the tool's own wording stay
                        // one keypress away, and `summary` is never rewritten.
                        let body = if adjacent {
                            label
                        } else {
                            format!("{tool_name}  {label}")
                        };
                        let raw = summary.clone();
                        let detail = Some(LineDetail::Folded {
                            line_count: raw.lines().count(),
                            char_count: raw.chars().count(),
                            raw,
                            // The meta row is a label, not the payload's head.
                            meta_head: MetaHead::None,
                        });
                        (format!("  {prefix}< {body}"), LineKind::ToolResult, detail)
                    } else {
                        let max_cols = self.transcript.wrap_width().saturating_sub(20).max(40);
                        // The result text is the tool's own answer and is never
                        // rewritten — only displayed. `raw` keeps it whole; the
                        // preview ladder decides how much of it to draw.
                        let raw = summary.clone();
                        let line_count = raw.lines().count();
                        let char_count = raw.chars().count();
                        // Fold only when there is something to fold: a lone
                        // short line stays a plain line, exactly as before.
                        let folds = !raw.is_empty() && (line_count > 1 || char_count > max_cols);
                        // The meta row is a *label*, not a restatement: just the
                        // answer's head line, abbreviated. The rest of the answer
                        // is payload for the preview ladder (`render.rs`), and
                        // `raw` keeps all of it — squashing the whole answer up
                        // here would leak past every tier and break the ladder.
                        let head = raw.lines().next().unwrap_or("");
                        let s = one_line(head, max_cols);
                        // The meta row *is* the answer's head (`  < head`),
                        // abbreviated to the line budget — record which, so
                        // the ladder neither draws the head twice nor loses
                        // the truncated tail at full expansion.
                        let meta_head = if s == head {
                            MetaHead::Whole
                        } else {
                            MetaHead::Abbreviated
                        };
                        // When the result sits directly under its own call the
                        // tool name is already on the line above — repeating it
                        // is chrome, not content. Out-of-order (parallel) results
                        // carry the name so the attachment stays readable.
                        let body = if s.is_empty() {
                            if adjacent {
                                String::new()
                            } else {
                                tool_name.clone()
                            }
                        } else if adjacent {
                            s
                        } else {
                            format!("{tool_name}  {s}")
                        };
                        let text = if body.is_empty() {
                            format!("  {prefix}<")
                        } else {
                            format!("  {prefix}< {body}")
                        };
                        let detail = if folds {
                            Some(LineDetail::Folded {
                                raw,
                                line_count,
                                char_count,
                                meta_head,
                            })
                        } else {
                            None
                        };
                        (text, LineKind::ToolResult, detail)
                    };
                    let line = OutputLine {
                        spans: None,
                        original: None,
                        detail,
                        text,
                        kind,
                        tool_state: None,
                    };
                    // Route to the correct transcript
                    if let Some(id) = agent_id.as_deref() {
                        if !id.is_empty() {
                            self.sub_agent_transcripts
                                .entry(id.to_string())
                                .or_default()
                                .push(line);
                        } else {
                            self.transcript.push(line);
                        }
                    } else {
                        self.transcript.push(line);
                    }
                }
                // Apply real file line numbers from tool metadata (edit_file returns edit_lines)
                if let Some(ref det) = details
                    && let Some(edit_lines) = det.get("edit_lines").and_then(|v| v.as_array())
                {
                    let offsets: Vec<u32> = edit_lines
                        .iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u32))
                        .collect();
                    // Find the most recent Diff in the transcript for this tool call
                    for line in self.transcript.output.iter_mut().rev() {
                        if let Some(LineDetail::Diff { hunks, .. }) = &mut line.detail {
                            for hunk in hunks.iter_mut() {
                                if let Some(&base) = offsets.get(hunk.edit_index)
                                    && base > 0
                                {
                                    let off = base - 1; // convert 1-based to offset
                                    for dl in &mut hunk.lines {
                                        if let Some(ref mut n) = dl.old_line {
                                            *n += off;
                                        }
                                        if let Some(ref mut n) = dl.new_line {
                                            *n += off;
                                        }
                                    }
                                    // Rebuild header with real line numbers
                                    let old_start =
                                        hunk.lines.iter().find_map(|l| l.old_line).unwrap_or(1);
                                    let new_start =
                                        hunk.lines.iter().find_map(|l| l.new_line).unwrap_or(1);
                                    let old_count =
                                        hunk.lines.iter().filter(|l| l.old_line.is_some()).count();
                                    let new_count =
                                        hunk.lines.iter().filter(|l| l.new_line.is_some()).count();
                                    hunk.header = format!(
                                        "@@ -{},{} +{},{} @@",
                                        old_start, old_count, new_start, new_count
                                    );
                                }
                            }
                            break; // only the most recent Diff
                        }
                    }
                }
                self.live_progress = None;
                self.note_activity(agent_id.as_deref(), Phase::Thinking);
            }
            RuntimeEvent::PlanUpdated {
                objective,
                explanation,
                plan,
                ..
            } => {
                self.flush_pending();

                // Build the new plan block: objective + status-marked steps + an
                // optional explanation. Steps are normalized to ≤60 chars by the
                // tool (fits one line); objective/explanation wrap with a hanging
                // indent.
                let mut block: Vec<OutputLine<BannerStyle>> = Vec::new();
                for (i, line) in wrap(&objective, self.transcript.wrap_width())
                    .into_iter()
                    .enumerate()
                {
                    let text = if i == 0 {
                        format!("📋 {line}")
                    } else {
                        format!("   {line}")
                    };
                    block.push(OutputLine {
                        spans: None,
                        original: None,
                        detail: None,
                        text,
                        kind: LineKind::Plan,
                        tool_state: None,
                    });
                }
                for item in &plan {
                    let marker = match item.status {
                        PlanStepStatus::Completed => "✅",
                        PlanStepStatus::InProgress => "🔄",
                        PlanStepStatus::Pending => "-",
                    };
                    block.push(OutputLine {
                        spans: None,
                        original: None,
                        detail: None,
                        text: format!("   {marker} {}", item.step),
                        kind: LineKind::Plan,
                        tool_state: None,
                    });
                }
                if let Some(exp) = &explanation {
                    let exp = exp.trim();
                    if !exp.is_empty() {
                        for (i, line) in wrap(exp, self.transcript.wrap_width())
                            .into_iter()
                            .enumerate()
                        {
                            let text = if i == 0 {
                                format!("   ↳ {line}")
                            } else {
                                format!("     {line}")
                            };
                            block.push(OutputLine {
                                spans: None,
                                original: None,
                                detail: None,
                                text,
                                kind: LineKind::Plan,
                                tool_state: None,
                            });
                        }
                    }
                }

                // Replace the previous block in place so it stays anchored where
                // it first appeared (the tool always sends the full plan).
                self.transcript.replace_plan(block);
            }
            RuntimeEvent::AwaitingApproval { request, .. } => {
                // Log line for history; the interactive popup (5b) is driven by
                // the approval queue, which the QueuedApprovalHandler feeds.
                self.flush_pending();
                self.transcript.push(OutputLine {
                    spans: None,
                    original: None,
                    detail: None,
                    text: format!("!! approval: {}", request.title),
                    kind: LineKind::Approval,
                    tool_state: None,
                });
                self.transcript.push(OutputLine {
                    spans: None,
                    original: None,
                    detail: None,
                    text: format!("     {}", request.message),
                    kind: LineKind::Approval,
                    tool_state: None,
                });
                self.begin_activity();
                self.status = AgentStatus::Running {
                    phase: Phase::AwaitingApproval,
                };
            }
            RuntimeEvent::RunFinished { agent_id, .. } => {
                self.flush_pending();
                // Only the root agent's finish ends the turn; sub-agents report
                // their own `RunFinished` with a non-empty `agent_id` (marking
                // that sub-agent done in the transcript + status strip).
                match agent_id.as_deref() {
                    Some(p) if !p.is_empty() => {
                        // Commit any pending child stream before the done marker.
                        let flushed = self.flush_child_stream(p);
                        self.sub_agent_transcripts
                            .entry(p.to_string())
                            .or_default()
                            .extend(flushed);
                        if let Some(state) = self.sub_agents.get_mut(p) {
                            state.status = SubAgentStatus::Done;
                            state.completed_at = Some(std::time::Instant::now());
                        }
                        // Route to sub-agent transcript
                        self.sub_agent_transcripts
                            .entry(p.to_string())
                            .or_default()
                            .push(OutputLine {
                                spans: None,
                                original: None,
                                detail: None,
                                text: format!("+ [{p}] done"),
                                kind: LineKind::Done,
                                tool_state: None,
                            });
                    }
                    _ => {
                        // Root agent: just flush and settle. The actual
                        // outcome message ("✅ done" / "⏳ waiting" / "❌ error")
                        // is rendered by TurnDone / TurnError after run_turn
                        // returns. With sub-agents or background tasks still
                        // running the panel stays alive — the fan-in batch
                        // injection / background wake continues the work.
                        // `running` is NOT cleared here: settle_after_turn
                        // (TurnDone/TurnError) is the sole clearer, so a late
                        // engine event can never un-flag the next turn.
                        self.settle_status_from_inflight();
                        tracing::info!(
                            follow_bottom = self.viewport.follow_bottom,
                            scroll_offset = self.viewport.scroll_offset,
                            output_len = self.transcript.len(),
                            "run_finished: view state"
                        );
                    }
                }
            }
            RuntimeEvent::RunCancelled { agent_id, .. } => {
                self.flush_pending();
                match agent_id.as_deref() {
                    Some(p) if !p.is_empty() => {
                        // Commit any pending child stream before the done marker.
                        let flushed = self.flush_child_stream(p);
                        self.sub_agent_transcripts
                            .entry(p.to_string())
                            .or_default()
                            .extend(flushed);
                        if let Some(state) = self.sub_agents.get_mut(p) {
                            state.status = SubAgentStatus::Done;
                            state.completed_at = Some(std::time::Instant::now());
                        }
                        // Route to sub-agent transcript
                        self.sub_agent_transcripts
                            .entry(p.to_string())
                            .or_default()
                            .push(OutputLine {
                                spans: None,
                                original: None,
                                detail: None,
                                text: format!("+ [{p}] done"),
                                kind: LineKind::Done,
                                tool_state: None,
                            });
                    }
                    _ => {
                        self.transcript.push(OutputLine {
                            spans: None,
                            original: None,
                            detail: None,
                            text: "⏹ cancelled".to_string(),
                            kind: LineKind::Cancelled,
                            tool_state: None,
                        });
                        // `running` clears via the turn's terminal event
                        // (settle_after_turn) — see the RunFinished note.
                        // Cancelling the turn does not cancel in-flight work:
                        // background shell tasks keep running, so they still
                        // gate the status (Waiting) and the later wake.
                        self.settle_status_from_inflight();
                        tracing::info!(
                            follow_bottom = self.viewport.follow_bottom,
                            scroll_offset = self.viewport.scroll_offset,
                            output_len = self.transcript.len(),
                            "cancel: view state"
                        );
                    }
                }
            }
            RuntimeEvent::UserEvent { event, .. } => match event {
                // Live tool progress (e.g. streaming `execute_command` output):
                // surface the latest line in the status bar. Full output is
                // still delivered as the tool's final summary, so this is
                // feedback only — no transcript pollution.
                UserEvent::Progress { text } => {
                    self.live_progress = Some(text);
                }
                // Engine-mechanism notices (Batch E): guard degradation,
                // reaper, ... Warning is a persistent red line that must NOT
                // settle the turn (the run may still be finishing); Progress
                // rides the status bar like tool progress; Info is a plain
                // output line.
                UserEvent::Notice { kind, source, text } => match kind {
                    NoticeKind::Warning => {
                        self.flush_pending();
                        self.push_error_line(&notice_line(&source, &text));
                    }
                    NoticeKind::Progress => {
                        self.live_progress = Some(text);
                    }
                    NoticeKind::Info => {
                        self.flush_pending();
                        // Gray system line: informational, visually distinct
                        // from both tool results and errors.
                        self.transcript.push_system(&notice_line(&source, &text));
                    }
                },
                _ => {}
            },
            // Tool-call arguments still streaming in (root only). The calls
            // themselves materialize only when the turn's stream drains, so
            // this is the sole liveness signal while a long brief is being
            // written — record it and let the status strip narrate the draft
            // instead of freezing on "streaming..." (session
            // 20261006_9264ba3e: ~25 s invisible while four spawn_agent
            // tasks streamed in).
            RuntimeEvent::ToolCallDraft {
                index,
                name,
                args_len,
                agent_id,
                ..
            } if agent_id.as_deref().filter(|id| !id.is_empty()).is_none() => {
                self.tool_drafts.insert(index, (name, args_len));
                self.note_activity(None, Phase::Streaming);
            }
            _ => {}
        }
    }
}
