//! Sub-agent task panel: lifecycle bookkeeping for the multi-agent fan-in.
//!
//! Split impl of `App` (methods live here, state stays on `App`): tracking
//! entries appear at spawn (registry snapshot reconciliation, Phase 5), flip
//! to done on child exit, and are reaped 3 s after completion — but never
//! while the root is busy or the user is inspecting the panel. Also owns the
//! per-child streaming buffers (each child gets its own stream state so two
//! interleaved children can't chop each other's pending text).

use std::time::{Duration, Instant};

use phi_agent::RegistrySnapshot;
use phi_kernel_tools::background_shell::BackgroundTaskStatus;
use phi_tui::lines::{LineKind, OutputLine};
use phi_tui::stream::StreamState;
use phi_tui::transcript::DEFAULT_WRAP_WIDTH;

use crate::banner::BannerStyle;

use super::app::{
    AgentStatus, App, BackgroundTaskEntry, FocusTarget, SubAgentState, SubAgentStatus,
};
use super::bg_wake::{MAX_TAIL_PER_TASK, may_reap};

/// How long a completed background task stays visible in the panel before
/// auto-reap. Shared by the reaper and the reconcile upsert guard (which
/// refuses to resurrect terminal snapshots already past this window) so the
/// two thresholds can never drift apart.
const BACKGROUND_REAP_AFTER: Duration = Duration::from_secs(3);

/// Live state for the thinking panel of the focused agent: pre-wrapped tail
/// lines (unprefixed — the panel title owns agent attribution), the agent id
/// when a child, and the pending thought's char count (token estimate).
pub(crate) struct ThinkingPanelState {
    pub(crate) lines: Vec<String>,
    pub(crate) agent: Option<String>,
    pub(crate) chars: usize,
}

impl App {
    /// Number of tracked sub-agents still `Running`.
    pub(crate) fn running_sub_agents(&self) -> usize {
        self.sub_agents
            .values()
            .filter(|s| s.status == SubAgentStatus::Running)
            .count()
    }

    /// Running background tasks split by deadline: `(bounded, daemons)`.
    /// Bounded (`timeout_ms > 0`) are jobs the agent waits on and reports;
    /// daemons (`timeout_ms == 0`) are servers that run until they die and
    /// must never hold a "waiting" state (see `BackgroundTaskEntry::is_indefinite`).
    pub(crate) fn bg_running_split(&self) -> (usize, usize) {
        let mut bounded = 0usize;
        let mut daemons = 0usize;
        for t in self.background_tasks.values() {
            if t.status != BackgroundTaskStatus::Running {
                continue;
            }
            if t.is_indefinite() {
                daemons += 1;
            } else {
                bounded += 1;
            }
        }
        (bounded, daemons)
    }

    /// A sub-agent finished (watcher Progress event): mark it done in the task
    /// panel and, while in the `Waiting` state, refresh the remaining count.
    pub fn mark_sub_agent_finished(&mut self, agent_path: &str) {
        if let Some(state) = self.sub_agents.get_mut(agent_path)
            && state.status == SubAgentStatus::Running
        {
            state.status = SubAgentStatus::Done;
            state.completed_at = Some(std::time::Instant::now());
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

    /// While `Waiting`, recompute both in-flight counts from the panel
    /// (sub-agents + background tasks); drop to `Idle` only when nothing
    /// *waitable* is left (the fan-in batch injection / background wake
    /// follows). Daemons don't keep the wait alive — only bounded jobs do.
    fn refresh_waiting_count(&mut self) {
        if let AgentStatus::Waiting { .. } = self.status {
            let n = self.running_sub_agents();
            let (bounded, daemons) = self.bg_running_split();
            self.status = if n > 0 || bounded > 0 {
                // `bg` stays "all running" so the counter never under-reports;
                // the wait itself is justified by bounded jobs only.
                AgentStatus::Waiting {
                    running: n,
                    bg: bounded + daemons,
                }
            } else {
                AgentStatus::Idle
            };
        }
    }

    /// Record a sub-agent's first appearance — emitting a `* [p] started` marker
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
            let name = p.split('/').next_back().unwrap_or(p).to_string();
            self.sub_agents.insert(
                p.to_string(),
                SubAgentState {
                    name,
                    status: SubAgentStatus::Running,
                    files: Vec::new(),
                    started_at: Instant::now(),
                    completed_at: None,
                    last_tool_at: Instant::now(),
                    events: Vec::new(),
                },
            );
            self.transcript.push(OutputLine {
                spans: None,
                original: None,
                detail: None,
                text: format!("* [{p}] started"),
                kind: LineKind::Tool,
                tool_state: None,
            });
        }
    }

    /// Whether the task panel should be shown: sub-agents only. Background
    /// shell tasks render as transcript tool-call records (launch line with
    /// `background: true`, completion via the bg-wake turn) plus the
    /// status-bar counter — a static command string never earned a row, and
    /// the rows weren't switchable views anyway (`render_output` only
    /// indexes `sub_agents`).
    pub fn should_show_task_panel(&self) -> bool {
        !self.sub_agents.is_empty()
    }

    /// Poll the background task registry for status updates (called every tick).
    ///
    /// Reconciles `background_tasks` against the registry's `snapshot_all()`.
    /// Also GCs completed tasks older than 3 seconds when the root is Idle.
    /// Returns `true` if any state changed (caller sets `dirty`).
    pub(crate) fn reconcile_background_tasks(&mut self) -> bool {
        // Owned clone: the sweep below (`gc`) must not hold a borrow of
        // `*self` across the `&mut self` calls in the upsert loop.
        let Some(registry) = self.background_registry.clone() else {
            return false;
        };

        // Consume-based GC with a 30-min fallback TTL (issue #35): entries
        // are swept once `task_output` has taken their output, or 30 min
        // after finish — whichever comes first. Never the old 5-min clock
        // that raced the wake's delivery.
        let gc_ttl = Duration::from_secs(1800);
        // Pure read first — `gc` runs *after* the upsert loop below, so a
        // terminal state is always observed before it can be swept away.
        let snapshots = registry.snapshot_all();
        let mut changed = false;

        // Upsert from snapshots
        let live_ids: std::collections::HashSet<String> =
            snapshots.iter().map(|s| s.id.clone()).collect();

        for snap in &snapshots {
            // Never (re-)insert a terminal snapshot whose `finished_at` is
            // already past the reap window. The registry keeps finished tasks
            // until its own much longer GC TTL expires, so once the panel has
            // shown and reaped a task, `snapshot_all` still reports it —
            // re-inserting it here gets it re-reaped on the same pass, every
            // tick (the reap → re-add → reap loop). This is safe because the
            // entry's `finished_at` IS the registry's timestamp and the reap
            // only ever fires at `BACKGROUND_REAP_AFTER` age: a snapshot past
            // that age has already been shown and reaped, or was never seen
            // before its display window closed. Entries still present keep
            // receiving status updates below.
            if !self.background_tasks.contains_key(&snap.id)
                && snap.status != BackgroundTaskStatus::Running
                && snap
                    .finished_at
                    .is_some_and(|at| at.elapsed() >= BACKGROUND_REAP_AFTER)
            {
                continue;
            }
            let entry = self.background_tasks.entry(snap.id.clone());
            let is_new = !matches!(entry, std::collections::btree_map::Entry::Occupied(_));
            entry.or_insert_with(|| BackgroundTaskEntry {
                id: snap.id.clone(),
                command: snap.command.clone(),
                timeout_ms: snap.timeout_ms,
                status: BackgroundTaskStatus::Running,
                started_at: snap.started_at,
                finished_at: None,
                reported: false,
                output_tail: String::new(),
                consumed: false,
            });

            let task = self.background_tasks.get_mut(&snap.id).unwrap();
            // Consumption flips independently of status (task_output fetch).
            if task.consumed != snap.consumed {
                task.consumed = snap.consumed;
                changed = true;
            }
            if task.status != snap.status {
                task.status = snap.status.clone();
                // Use the snapshot's finished_at instead of Instant::now()
                // to preserve the real completion time across reap-reconcile cycles.
                task.finished_at = snap.finished_at;
                if snap.status != BackgroundTaskStatus::Running {
                    // First terminal observation: snapshot the output tail so
                    // the wake can report even after the registry entry is
                    // GC'd (consume-based) — one capture, not per tick.
                    task.output_tail = snap.output_tail(MAX_TAIL_PER_TASK);
                }
                changed = true;
                // A background task just started or (more importantly) ended:
                // while `Waiting` the status-bar counts must follow, and the
                // last one flipping to terminal is what arms the bg wake.
                self.refresh_waiting_count();
            }
            if is_new {
                changed = true;
            }
        }

        // GC only after every snapshot above has been observed.
        registry.gc(gc_ttl);

        // Remove entries that were GC'd from the registry — but never one
        // that still owes the agent a wake report (see `may_reap`). The
        // registry's consume-GC can drop an entry whose outcome has not been
        // reported yet; the map keeps it until the wake delivers.
        let stale_ids: Vec<String> = self
            .background_tasks
            .iter()
            .filter(|(id, t)| !live_ids.contains(*id) && may_reap(t))
            .map(|(id, _)| id.clone())
            .collect();
        for id in stale_ids {
            self.background_tasks.remove(&id);
            changed = true;
        }

        // Auto-reap completed background tasks (Done/TimedOut/Cancelled/Error)
        // after 3 seconds, but only when root is Idle and not inspecting.
        // We reap regardless of whether the task is still in the registry,
        // because the registry's GC TTL (5 min) is much longer than our
        // desired panel display time (3s).
        if matches!(self.status, AgentStatus::Idle)
            && !matches!(self.task_panel.focus, FocusTarget::TaskList(_))
        {
            let now = Instant::now();
            let reap_ids: Vec<String> = self
                .background_tasks
                .iter()
                .filter(|(_, t)| {
                    t.status != BackgroundTaskStatus::Running
                        && t.finished_at
                            .is_some_and(|at| now.duration_since(at) >= BACKGROUND_REAP_AFTER)
                        && may_reap(t)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if !reap_ids.is_empty() {
                tracing::info!(
                    reap_count = reap_ids.len(),
                    reap_ids = ?reap_ids,
                    "reconcile: reaping background tasks"
                );
            }
            for id in reap_ids {
                self.background_tasks.remove(&id);
                changed = true;
            }
        }

        changed
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
                // Creates the entry (with the `* started` transcript marker)
                // at spawn time — before the child's first tool call, which
                // is when the event-driven path used to discover it.
                self.track_agent(Some(&agent.path));
            }
            if let Some(state) = self.sub_agents.get_mut(&agent.path) {
                match (&state.status, running_fact) {
                    (SubAgentStatus::Running, false) => {
                        state.status = SubAgentStatus::Done;
                        state.completed_at = Some(Instant::now());
                    }
                    (SubAgentStatus::Done, true) => {
                        // Re-tasked (send_message trigger): reopen the entry.
                        state.status = SubAgentStatus::Running;
                        state.completed_at = None;
                    }
                    _ => {}
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
        let ids_to_remove: Vec<String> = self
            .sub_agents
            .iter()
            .filter(|(_, state)| {
                state.status == SubAgentStatus::Done
                    && state
                        .completed_at
                        .is_some_and(|at| now.duration_since(at).as_secs() >= 3)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in ids_to_remove {
            self.sub_agents.remove(&id);
            self.sub_agent_transcripts.remove(&id);
            self.child_streams.remove(&id);
            self.thinking_since.remove(&id);
            removed = true;
        }

        // Reset focus if it's now out of bounds. The panel lists sub-agents
        // only — background tasks are not focusable rows.
        let total = self.sub_agents.len();
        if let FocusTarget::TaskList(index) = &self.task_panel.focus
            && *index >= total
        {
            self.task_panel.focus = if total == 0 {
                FocusTarget::Input
            } else {
                FocusTarget::TaskList(total - 1)
            };
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
        // Root flush ⇒ the root segment is over. Other streams' timers are
        // keyed separately and survive structural events that only flush the
        // root (fixes: child B's timer used to die on child A's RunFinished).
        self.thinking_since.remove("");
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
    pub(crate) fn push_child_thought(
        &mut self,
        id: &str,
        text: &str,
    ) -> Vec<OutputLine<BannerStyle>> {
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
            Some(st) => {
                let lines = st.flush();
                self.thinking_since.remove(id);
                lines
            }
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

    /// Live state for the thinking panel of the **focused** agent. `None` when
    /// the focused stream has no pending THOUGHT — only thought tails panel;
    /// pending prose keeps streaming inline. Root looks at `stream`, a focused
    /// child at its `child_streams` entry. Body lines are UNPREFIXED: the panel
    /// title already names the agent (`thinking - <id>`), and the `[<id>] `
    /// prefix is only for stream lines that land in the shared transcript.
    pub(crate) fn thinking_panel_state(&self) -> Option<ThinkingPanelState> {
        match &self.task_panel.focus {
            FocusTarget::Input => {
                if !self.stream.has_pending_thought() {
                    return None;
                }
                let (lines, _) = self.stream.tail_lines()?;
                let chars = self
                    .stream
                    .tail_raw()
                    .map_or(0, |(raw, _)| raw.chars().count());
                Some(ThinkingPanelState {
                    lines: lines.to_vec(),
                    agent: None,
                    chars,
                })
            }
            FocusTarget::TaskList(index) => {
                let id = self.sub_agents.keys().nth(*index).cloned()?;
                let stream = self.child_streams.get(&id)?;
                let (lines, kind) = stream.tail_lines()?;
                if kind != LineKind::Thought {
                    return None;
                }
                let chars = stream.tail_raw().map_or(0, |(raw, _)| raw.chars().count());
                Some(ThinkingPanelState {
                    lines: lines.to_vec(),
                    agent: Some(id),
                    chars,
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "task_panel_tests.rs"]
mod task_panel_tests;
