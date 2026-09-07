//! Sub-agent task panel: lifecycle bookkeeping for the multi-agent fan-in.
//!
//! Split impl of `App` (methods live here, state stays on `App`): tracking
//! entries appear at spawn (registry snapshot reconciliation, Phase 5), flip
//! to done on child exit, and are reaped 3 s after completion — but never
//! while the root is busy or the user is inspecting the panel. Also owns the
//! per-child streaming buffers (each child gets its own stream state so two
//! interleaved children can't chop each other's pending text).

use std::time::Instant;

use phi_agent::RegistrySnapshot;
use phi_tui::lines::{LineKind, OutputLine};
use phi_tui::stream::StreamState;
use phi_tui::transcript::DEFAULT_WRAP_WIDTH;

use crate::banner::BannerStyle;

use super::app::{AgentStatus, App, FocusTarget, SubAgentState, SubAgentStatus};

impl App {
    /// Number of tracked sub-agents still `Running`.
    pub(crate) fn running_sub_agents(&self) -> usize {
        self.sub_agents.values().filter(|s| s.status == SubAgentStatus::Running).count()
    }

    /// A sub-agent finished (watcher Progress event): mark it done in the task
    /// panel and, while in the `Waiting` state, refresh the remaining count.
    pub fn mark_sub_agent_finished(&mut self, agent_path: &str) {
        if let Some(state) = self.sub_agents.get_mut(agent_path) {
            if state.status == SubAgentStatus::Running {
                state.status = SubAgentStatus::Done;
                state.completed_at = Some(std::time::Instant::now());
            }
        }
        self.refresh_waiting_count();
    }

    /// All pending sub-agents finished (batch injection): mark every still-
    /// running entry done so the 3s auto-cleanup can sweep the panel.
    pub fn mark_all_sub_agents_finished(&mut self) {
        for state in self.sub_agents.values_mut() {
            if state.status == SubAgentStatus::Running {
                state.status = SubAgentStatus::Done;
                state.completed_at = Some(std::time::Instant::now());
            }
        }
        self.refresh_waiting_count();
    }

    /// While `Waiting`, recompute the running count from the panel; drop to
    /// `Idle` when the last sub-agent finished (the batch injection follows).
    fn refresh_waiting_count(&mut self) {
        if let AgentStatus::Waiting { .. } = self.status {
            let n = self.running_sub_agents();
            self.status = if n > 0 {
                AgentStatus::Waiting { running: n }
            } else {
                AgentStatus::Idle
            };
        }
    }

    /// Record a sub-agent's first appearance — emitting a `⏺ [p] started` marker
    /// and registering it in `sub_agents` — so the transcript and status strip
    /// show its lifecycle. No-op for the root agent (`agent_id == None`) and for
    /// sub-agents already seen this turn.
    pub(crate) fn track_agent(&mut self, agent_id: Option<&str>) {
        let Some(p) = agent_id.filter(|p| !p.is_empty()) else {
            return;
        };
        if !self.sub_agents.contains_key(p) {
            // Any pending root text precedes the first sub-agent event, so flush
            // it before the `started` marker to keep transcript order correct.
            self.flush_pending();
            // Extract task name from agent_id (format: "root/<task_name>")
            let name = p.split('/').last().unwrap_or(p).to_string();
            self.sub_agents.insert(p.to_string(), SubAgentState {
                name,
                status: SubAgentStatus::Running,
                files: Vec::new(),
                started_at: Instant::now(),
                completed_at: None,
                last_tool_at: Instant::now(),
                events: Vec::new(),
            });
            self.transcript.push(OutputLine { spans: None, original: None,
                detail: None,
                text: format!("⏺ [{p}] started"),
                kind: LineKind::Tool,
            });
        }
    }

    /// Whether the task panel should be shown (has active sub-agents).
    pub fn should_show_task_panel(&self) -> bool {
        !self.sub_agents.is_empty()
    }

    /// Reconcile the task panel against the registry's authoritative fact
    /// snapshot (Phase 5: `subscribe_lifecycle` watch → `TuiEvent::Lifecycle`).
    ///
    /// Mapping (design doc `agent-state-machine-design.md` Phase 5):
    /// `queued`/`running` → panel `Running`, `done` → panel `Done`,
    /// disappearance (unregistration: normal exit after posting, or close)
    /// → panel `Done`, never removal — the 3s reaper owns removal so the
    /// user keeps the review window. Unknown `done` agents are ignored: a
    /// freshly registered agent reads `done` until its first `send_task`,
    /// and entries should appear with the first fact of real work, not as
    /// phantom dones.
    pub(crate) fn apply_lifecycle_snapshot(&mut self, snap: &RegistrySnapshot) {
        for agent in &snap.agents {
            let running_fact = matches!(agent.status.as_str(), "queued" | "running");
            if running_fact {
                // Creates the entry (with the `⏺ started` transcript marker)
                // at spawn time — before the child's first tool call, which
                // is when the event-driven path used to discover it.
                self.track_agent(Some(&agent.path));
            }
            if let Some(state) = self.sub_agents.get_mut(&agent.path) {
                match (&state.status, running_fact) {
                    (SubAgentStatus::Running, false) => {
                        state.status = SubAgentStatus::Done;
                        state.completed_at = Some(Instant::now());
                    },
                    (SubAgentStatus::Done, true) => {
                        // Re-tasked (send_message trigger): reopen the entry.
                        state.status = SubAgentStatus::Running;
                        state.completed_at = None;
                    },
                    _ => {},
                }
            }
        }
        // Tracked entries absent from the snapshot have unregistered.
        let live: std::collections::HashSet<&str> =
            snap.agents.iter().map(|a| a.path.as_str()).collect();
        for (path, state) in self.sub_agents.iter_mut() {
            if !live.contains(path.as_str()) && state.status == SubAgentStatus::Running {
                state.status = SubAgentStatus::Done;
                state.completed_at = Some(Instant::now());
            }
        }
        self.refresh_waiting_count();
    }

    /// Remove completed sub-agents that have been done for more than 3 seconds.
    /// Returns true if any agents were removed.
    pub fn cleanup_completed_agents(&mut self) -> bool {
        // Only reap when the root is fully Idle. While the root is Waiting
        // (children still running) the finished children's transcripts are the
        // only place the user can review what they did — deleting them 3s after
        // completion made sub-agent execution invisible (session 20260903 fan-in
        // made "turn ends while children run" the norm, so this fired constantly).
        if !matches!(self.status, AgentStatus::Idle) {
            return false;
        }
        // Likewise while the user is inspecting the task list: reaping under
        // their eyes is exactly the "can't see what the sub-agent did" bug.
        // Cleanup resumes once focus returns to the input box.
        if matches!(self.task_panel.focus, FocusTarget::TaskList(_)) {
            return false;
        }
        let now = Instant::now();
        let mut removed = false;
        let ids_to_remove: Vec<String> = self.sub_agents.iter()
            .filter(|(_, state)| {
                state.status == SubAgentStatus::Done &&
                state.completed_at.map_or(false, |at| now.duration_since(at).as_secs() >= 3)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in ids_to_remove {
            self.sub_agents.remove(&id);
            self.sub_agent_transcripts.remove(&id);
            self.child_streams.remove(&id);
            removed = true;
        }

        // Reset focus if it's now out of bounds
        if let FocusTarget::TaskList(index) = &self.task_panel.focus {
            if *index >= self.sub_agents.len() {
                self.task_panel.focus = if self.sub_agents.is_empty() {
                    FocusTarget::Input
                } else {
                    FocusTarget::TaskList(self.sub_agents.len() - 1)
                };
            }
        }

        removed
    }

    /// Commit any pending streaming text/thought into the transcript (thought
    /// first, then prose). Called on structural events and turn end.
    pub(crate) fn flush_pending(&mut self) {
        let had_text = self.stream.has_pending_text();
        let had_thought = self.stream.has_pending_thought();
        // Get the pending agent BEFORE flushing (flush takes it)
        let pending_agent = self.stream.pending_agent().map(|s| s.to_string());
        let flushed = self.stream.flush();
        // Route flushed lines to the correct transcript based on pending_agent
        for line in flushed {
            if let Some(ref agent_id) = pending_agent {
                if !agent_id.is_empty() {
                    self.sub_agent_transcripts
                        .entry(agent_id.clone())
                        .or_default()
                        .push(line);
                } else {
                    self.transcript.push(line);
                }
            } else {
                self.transcript.push(line);
            }
        }
        if had_text || had_thought {
            tracing::info!(
                follow_bottom = self.viewport.follow_bottom,
                scroll_offset = self.viewport.scroll_offset,
                output_len = self.transcript.len(),
                had_text,
                had_thought,
                "flush_pending: view state"
            );
        }
    }

    // ── Per-child streaming ───────────────────────────────────────────────

    /// Accumulate a child's prose delta into its own stream buffer, returning
    /// any lines the flush committed (caller routes them into the child's
    /// transcript). Lines are prefixed `[<id>] ` like the pre-split behavior.
    pub(crate) fn push_child_text(&mut self, id: &str, text: &str) -> Vec<OutputLine<BannerStyle>> {
        self.child_streams
            .entry(id.to_string())
            .or_insert_with(|| StreamState::new(DEFAULT_WRAP_WIDTH))
            .push_text(text, Some(id))
    }

    /// Accumulate a child's reasoning delta into its own stream buffer.
    pub(crate) fn push_child_thought(&mut self, id: &str, text: &str) -> Vec<OutputLine<BannerStyle>> {
        self.child_streams
            .entry(id.to_string())
            .or_insert_with(|| StreamState::new(DEFAULT_WRAP_WIDTH))
            .push_thought(text, Some(id))
    }

    /// Commit a child's pending text/thought into whole lines (e.g. before its
    /// tool-invocation line or its `done` marker, so ordering inside the
    /// child's transcript stays chronological). Empty when the child has no
    /// pending stream content.
    pub(crate) fn flush_child_stream(&mut self, id: &str) -> Vec<OutputLine<BannerStyle>> {
        match self.child_streams.get_mut(id) {
            Some(st) => st.flush(),
            None => Vec::new(),
        }
    }

    /// A child's live streaming tail for the focused view: `(text, kind)` with
    /// the `[<id>] ` first-line prefix already applied, matching the shape the
    /// eventual committed line will have (`StreamState::flush_text`). Owned
    /// copy — keeps `render_output`'s borrow graph simple at ~1 clone/frame.
    pub fn child_stream_tail_raw(&self, id: &str) -> Option<(String, LineKind)> {
        let (raw, kind) = self.child_streams.get(id)?.tail_raw()?;
        Some((format!("[{id}] {raw}"), kind))
    }

    /// A child's live tail as pre-wrapped lines (the Thought-fallback path),
    /// first line prefixed like the committed lines will be.
    pub fn child_stream_tail_lines(&self, id: &str) -> Option<(Vec<String>, LineKind)> {
        let (lines, kind) = self.child_streams.get(id)?.tail_lines()?;
        let mut lines = lines.to_vec();
        if let Some(first) = lines.first_mut() {
            *first = format!("[{id}] {first}");
        }
        Some((lines, kind))
    }
}

#[cfg(test)]
#[path = "task_panel_tests.rs"]
mod task_panel_tests;
