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

use crate::ui::app::{App, AgentStatus, LineKind, OutputLine, Phase, SubAgentStatus};
use crate::ui::wrap::{one_line, wrap};

/// Prefix for sub-agent output lines (e.g., `[task_name] `).
fn agent_prefix(agent_id: Option<&str>) -> String {
    match agent_id {
        Some(id) if !id.is_empty() => format!("[{id}] "),
        _ => String::new(),
    }
}

impl App {
    /// Handle a runtime event from the agent.
    pub fn handle_runtime(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::TextDelta { text, agent_id, .. } => {
                self.track_agent(agent_id.as_deref());
                let flushed = self.stream.push_text(&text, agent_id.as_deref());
                self.transcript.extend(flushed);
                self.status = AgentStatus::Running {
                    phase: Phase::Streaming,
                };
            }
            RuntimeEvent::ThoughtDelta { text, agent_id, .. } => {
                self.track_agent(agent_id.as_deref());
                let flushed = self.stream.push_thought(&text, agent_id.as_deref());
                self.transcript.extend(flushed);
                self.status = AgentStatus::Running {
                    phase: Phase::Thinking,
                };
            }
            RuntimeEvent::ToolCallStarted {
                tool_name,
                args_json,
                agent_id,
                ..
            } => {
                self.flush_pending();
                // Fresh tool call → drop any progress from the previous one.
                self.live_progress = None;
                // Tools render inline in the transcript (Claude Code style): an
                // invocation line now, a result line on `ToolCallFinished`.
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());
                // `update_plan` renders as a plan block (see `PlanUpdated`), so both
                // its invocation line (raw JSON args) and result line are noise —
                // suppress them. Its status transition is still applied.
                if tool_name != "update_plan" {
                    let max_cols = self.transcript.wrap_width().saturating_sub(20).max(40);
                    let args = one_line(&args_json, max_cols);
                    let text = if args.is_empty() {
                        format!("⏺ {prefix}{tool_name}")
                    } else {
                        format!("⏺ {prefix}{tool_name} {args}")
                    };
                    self.transcript.push(OutputLine { spans: None, original: None,
                        text,
                        kind: LineKind::Tool,
                    });
                }
                self.status = AgentStatus::Running {
                    phase: Phase::ToolCall { tool: tool_name },
                };
            }
            RuntimeEvent::ToolCallFinished {
                tool_name,
                summary,
                denied,
                agent_id,
                ..
            } => {
                self.track_agent(agent_id.as_deref());
                let prefix = agent_prefix(agent_id.as_deref());
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
                    self.transcript.push(OutputLine { spans: None, original: None, text, kind });
                }
                self.live_progress = None;
                self.status = AgentStatus::Running {
                    phase: Phase::Thinking,
                };
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
                    block.push(OutputLine { spans: None, original: None, text, kind: LineKind::Plan });
                }
                for item in &plan {
                    let marker = match item.status {
                        PlanStepStatus::Completed => "✅",
                        PlanStepStatus::InProgress => "🔄",
                        PlanStepStatus::Pending => "○",
                    };
                    block.push(OutputLine { spans: None, original: None,
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
                            block.push(OutputLine { spans: None, original: None, text, kind: LineKind::Plan });
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
                    text: format!("⚠️  approval: {}", request.title),
                    kind: LineKind::Approval,
                });
                self.transcript.push(OutputLine { spans: None, original: None,
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
                        self.sub_agents.insert(p.to_string(), SubAgentStatus::Done);
                        self.transcript.push(OutputLine { spans: None, original: None,
                            text: format!("✓ [{p}] done"),
                            kind: LineKind::Done,
                        });
                    }
                    _ => {
                        // Root agent: just flush and set idle.  The actual
                        // outcome message ("✅ done" or "❌ error") is rendered
                        // by TurnDone / TurnError after run_turn returns.
                        self.status = AgentStatus::Idle;
                        self.running = false;
                        self.sub_agents.clear();
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
                        self.sub_agents.insert(p.to_string(), SubAgentStatus::Done);
                        self.transcript.push(OutputLine { spans: None, original: None,
                            text: format!("✓ [{p}] done"),
                            kind: LineKind::Done,
                        });
                    }
                    _ => {
                        self.transcript.push(OutputLine { spans: None, original: None,
                            text: "⏹ cancelled".to_string(),
                            kind: LineKind::Cancelled,
                        });
                        self.status = AgentStatus::Idle;
                        self.running = false;
                        self.sub_agents.clear();
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
