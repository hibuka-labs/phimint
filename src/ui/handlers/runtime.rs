//! Runtime event handler for the TUI application.
//!
//! Handles `RuntimeEvent` from the agent, updating the application state
//! accordingly. This includes:
//! - Text streaming (TextDelta, ThoughtDelta)
//! - Tool calls (ToolCallStarted, ToolCallFinished)
//! - Plan updates (PlanUpdated)
//! - Approval requests (AwaitingApproval)
//! - Run lifecycle (RunFinished, RunCancelled)
//! - User events (Progress)

use agent_base::{PlanStepStatus, UserEvent};
use phi_agent::RuntimeEvent;

use crate::ui::app::{App, AgentStatus, DiffHunk, LineKind, OutputLine, Phase, SubAgentStatus, ToolDetail, ToolEvent};
use crate::ui::diff::{diff_to_hunks, diff_to_hunks_indexed};
use crate::ui::wrap::{one_line, wrap};

/// Build a `ToolDetail::Diff` from a file tool's `args_json`, or `None` if
/// the tool is not a file-edit tool or parsing fails.
fn build_tool_detail(tool_name: &str, args_json: &str) -> Option<ToolDetail> {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(tool = tool_name, error = %e, "build_tool_detail: failed to parse args_json");
            return None;
        }
    };
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return None,
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
            Some(ToolDetail::Diff { path, hunks: all_hunks })
        }
        "write_file" => {
            let content = match args.get("content").and_then(|v| v.as_str()) {
                Some(c) => c,
                None => return None,
            };
            // Always show all-Add diff for write_file (what's being written).
            // For overwrite, the old file may already exist on disk but we
            // don't want to diff against it — the tool result handles that.
            let hunks = diff_to_hunks("", content, 3);
            if hunks.is_empty() {
                return None;
            }
            Some(ToolDetail::Diff { path, hunks })
        }
        _ => None,
    }
}

/// Prefix for sub-agent output lines (e.g., `[task_name] `).
fn agent_prefix(agent_id: Option<&str>) -> String {
    match agent_id {
        Some(id) if !id.is_empty() => format!("[{id}] "),
        _ => String::new(),
    }
}

impl App {
    /// Reflect streaming activity in the status strip — root events only.
    /// Child events arrive through the persistent bus subscription even while
    /// the root is Waiting/Idle; they must not flip the root's state back to
    /// Running.
    fn note_activity(&mut self, agent_id: Option<&str>, phase: Phase) {
        if agent_id.map_or(true, |id| id.is_empty()) {
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
                        let flushed = self.push_child_text(id, &text);
                        self.sub_agent_transcripts
                            .entry(id.to_string())
                            .or_default()
                            .extend(flushed);
                    }
                    None => {
                        let flushed = self.stream.push_text(&text, agent_id.as_deref());
                        for line in flushed {
                            self.transcript.push(line);
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
                        let flushed = self.push_child_thought(id, &text);
                        self.sub_agent_transcripts
                            .entry(id.to_string())
                            .or_default()
                            .extend(flushed);
                    }
                    None => {
                        let flushed = self.stream.push_thought(&text, agent_id.as_deref());
                        for line in flushed {
                            self.transcript.push(line);
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
                // Tools render inline in the transcript (Claude Code style): an
                // invocation line now, a result line on `ToolCallFinished`.
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());
                // Update SubAgentState events
                if let Some(id) = agent_id.as_deref() {
                    if let Some(state) = self.sub_agents.get_mut(id) {
                        state.events.push(ToolEvent {
                            tool_name: tool_name.clone(),
                            summary: String::new(),
                            is_finished: false,
                        });
                    }
                }
                // `update_plan` renders as a plan block (see `PlanUpdated`), so both
                // its invocation line (raw JSON args) and result line are noise —
                // suppress them. Its status transition is still applied.
                if tool_name != "update_plan" {
                    // For file-edit tools, build an inline diff and attach it as
                    // structured detail. The invocation line shows a compact summary;
                    // the renderer expands the diff block below it.
                    let detail = build_tool_detail(&tool_name, &args_json);
                    let max_cols = self.transcript.wrap_width().saturating_sub(20).max(40);
                    let args = one_line(&args_json, max_cols);
                    let text = if args.is_empty() {
                        format!("⏺ {prefix}{tool_name}")
                    } else {
                        format!("⏺ {prefix}{tool_name} {args}")
                    };
                    let line = OutputLine { spans: None, original: None,
                detail,
                        text,
                        kind: LineKind::Tool,
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
                if let Some(id) = agent_id.as_deref() {
                    if let Some(state) = self.sub_agents.get_mut(id) {
                        // Find the last unfinished event and mark it as finished
                        if let Some(event) = state.events.iter_mut().rev().find(|e| !e.is_finished) {
                            event.summary = summary.clone();
                            event.is_finished = true;
                        }
                    }
                }
                // `update_plan` renders as a plan block (see `PlanUpdated`), so its
                // tool-result line is redundant — suppress it. Everything else
                // still finalizes the tool-progress state.
                if tool_name != "update_plan" {
                    let (text, kind) = if denied {
                        (format!("  {prefix}⛔ {tool_name} denied"), LineKind::Error)
                    } else {
                        let max_cols = self.transcript.wrap_width().saturating_sub(20).max(40);
                        let s = one_line(&summary, max_cols);
                        let text = if s.is_empty() {
                            format!("  {prefix}✓ {tool_name}")
                        } else {
                            format!("  {prefix}✓ {tool_name} {s}")
                        };
                        (text, LineKind::ToolResult)
                    };
                    let line = OutputLine { spans: None, original: None, detail: None, text, kind };
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
                if let Some(ref det) = details {
                    if let Some(edit_lines) = det.get("edit_lines").and_then(|v| v.as_array()) {
                        let offsets: Vec<u32> = edit_lines
                            .iter()
                            .filter_map(|v| v.as_u64().map(|n| n as u32))
                            .collect();
                        // Find the most recent Diff in the transcript for this tool call
                        for line in self.transcript.output.iter_mut().rev() {
                            if let Some(ToolDetail::Diff { hunks, .. }) = &mut line.detail {
                                for hunk in hunks.iter_mut() {
                                    if let Some(&base) = offsets.get(hunk.edit_index) {
                                        if base > 0 {
                                            let off = base - 1; // convert 1-based to offset
                                            for dl in &mut hunk.lines {
                                                if let Some(ref mut n) = dl.old_line { *n += off; }
                                                if let Some(ref mut n) = dl.new_line { *n += off; }
                                            }
                                            // Rebuild header with real line numbers
                                            let old_start = hunk.lines.iter().find_map(|l| l.old_line).unwrap_or(1);
                                            let new_start = hunk.lines.iter().find_map(|l| l.new_line).unwrap_or(1);
                                            let old_count = hunk.lines.iter().filter(|l| l.old_line.is_some()).count();
                                            let new_count = hunk.lines.iter().filter(|l| l.new_line.is_some()).count();
                                            hunk.header = format!("@@ -{},{} +{},{} @@", old_start, old_count, new_start, new_count);
                                        }
                                    }
                                }
                                break; // only the most recent Diff
                            }
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
                let mut block: Vec<OutputLine> = Vec::new();
                for (i, line) in wrap(&objective, self.transcript.wrap_width()).into_iter().enumerate() {
                    let text = if i == 0 {
                        format!("📋 {line}")
                    } else {
                        format!("   {line}")
                    };
                    block.push(OutputLine { spans: None, original: None, detail: None, text, kind: LineKind::Plan });
                }
                for item in &plan {
                    let marker = match item.status {
                        PlanStepStatus::Completed => "✅",
                        PlanStepStatus::InProgress => "🔄",
                        PlanStepStatus::Pending => "○",
                    };
                    block.push(OutputLine { spans: None, original: None,
                detail: None,
                        text: format!("   {marker} {}", item.step),
                        kind: LineKind::Plan,
                    });
                }
                if let Some(exp) = &explanation {
                    let exp = exp.trim();
                    if !exp.is_empty() {
                        for (i, line) in wrap(exp, self.transcript.wrap_width()).into_iter().enumerate() {
                            let text = if i == 0 {
                                format!("   ↳ {line}")
                            } else {
                                format!("     {line}")
                            };
                            block.push(OutputLine { spans: None, original: None, detail: None, text, kind: LineKind::Plan });
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
                self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                    text: format!("⚠️  approval: {}", request.title),
                    kind: LineKind::Approval,
                });
                self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                    text: format!("     {}", request.message),
                    kind: LineKind::Approval,
                });
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
                            .push(OutputLine { spans: None, original: None,
                detail: None,
                                text: format!("✓ [{p}] done"),
                                kind: LineKind::Done,
                            });
                    }
                    _ => {
                        // Root agent: just flush and set idle.  The actual
                        // outcome message ("✅ done" / "⏳ waiting" / "❌ error")
                        // is rendered by TurnDone / TurnError after run_turn
                        // returns. With sub-agents still running the panel stays
                        // alive — the fan-in batch injection continues the work.
                        self.status = AgentStatus::Idle;
                        self.running = false;
                        if self.sub_agents.values().any(|s| s.status == SubAgentStatus::Running) {
                            self.status = AgentStatus::Waiting {
                                running: self.sub_agents.values()
                                    .filter(|s| s.status == SubAgentStatus::Running).count(),
                            };
                        } else {
                            self.sub_agents.clear();
                            self.sub_agent_transcripts.clear();
                            self.child_streams.clear();
                        }
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
                            .push(OutputLine { spans: None, original: None,
                detail: None,
                                text: format!("✓ [{p}] done"),
                                kind: LineKind::Done,
                            });
                    }
                    _ => {
                        self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                            text: "⏹ cancelled".to_string(),
                            kind: LineKind::Cancelled,
                        });
                        self.status = AgentStatus::Idle;
                        self.running = false;
                        self.sub_agents.clear();
                        self.sub_agent_transcripts.clear();
                        self.child_streams.clear();
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
                _ => {}
            },
            _ => {}
        }
    }
}
