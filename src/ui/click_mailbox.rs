//! Click mailbox (M2): browser clicks → boundary → synthesized user message.
//!
//! The hardest half of the design canvas, deliberately isolated from the TUI:
//! clicks are *user answers*, so they must arrive as a real user turn — never
//! injected into an active one (same failure class as the fan-in injection
//! bug). This module owns the policy; `run.rs` only executes side effects.
//!
//! Boundary semantics (M2a — borrowed from `bg_wake` only the debt-not-lost +
//! consume-once accounting, never its 15s quiet window):
//!
//! - **busy** (root turn in flight): clicks accumulate here — queued, never
//!   dropped, never injected. The turn end is the boundary ("turn 一结束立刻
//!   drain").
//! - **idle**: the boundary is at most one coalesce window away. Single-select
//!   rapid re-clicks inside the window report only the last pick (M2b).
//! - **Confirm / Skip** freeze the answer immediately (no window — the user
//!   said they are done; the window is for single-select re-clicks only).
//! - **Enter** is a boundary of its own (M2e): already-pending picks merge
//!   behind the typed text as one message. Drain never touches the composer
//!   buffer; the merge is composed only at the Enter submit site.
//!
//! Synthesis is locked by M2d — every form carries the `(画布选择)` prefix so
//! transcript and (later) design-decision records can tell browser picks from
//! typed text:
//!
//! | case | text |
//! |---|---|
//! | single | `(画布选择) 已选 B · 侧边栏导航` |
//! | multiselect confirm | `(画布选择) 已选 A、C · 侧边栏导航, 混合式` |
//! | correction after submit | `(画布选择) 更正：已选 C · 混合式` |
//! | skip | `(画布选择) 跳过 · 不做选择` |
//! | display-only (`options: []`) | no message, clicks carry no semantics |
//!
//! Drafts are synthesized at the boundary, not at click time: an unsent pick
//! is still mutable ("连点窗内只报最后一次"), and `更正` marks only screens that
//! already sent an answer ("提交后再点同一屏"). Multi-select toggle sets stay
//! live until Confirm freezes them (M2b: "点「确认」才提交").

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use phi_canvas::{CanvasEvent, EventAction, ScreenEnvelope};

/// M2d's answer marker: every synthesized pick line starts with it. Also the
/// 1C waiting summary's filter — merged submits (M2e) can bury picks behind
/// typed text, and the prefix is what finds them again.
pub(crate) const ANSWER_PREFIX: &str = "(画布选择)";

/// M2b's rapid-click coalesce window (single-select only; M2b exempts
/// multiselect). The spec names it a "1 秒连点窗" but also budgets click→echo
/// at ≤1s, and a literal 1s window plus loop-tick latency overshoots that
/// budget — so the window is trimmed to leave slack. Re-clicks inside the
/// window extend it (anchored on the newest click: "只报最后一次").
pub(crate) const CLICK_COALESCE: Duration = Duration::from_millis(800);

/// One selectable option as the mailbox needs it: id → label for synthesis.
type OptionPair = (String, String);

/// A screen's unsent answer (M2). Synthesized only when a boundary consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Draft {
    /// Single-select latest pick, waiting out [`CLICK_COALESCE`].
    Single {
        option_id: String,
        clicked_at: Instant,
    },
    /// Multiselect Confirm: the toggle set *as of the confirm click* (frozen —
    /// later toggles are correction material, not part of this answer).
    Confirmed { ids: Vec<String> },
    /// Skip / nothing chosen (also an empty Confirm).
    Skipped,
}

#[derive(Debug)]
struct ScreenState {
    /// Envelope seen? Unknown screens (host restarted, event before lookup)
    /// fall back to single-select semantics with the id doubling as label —
    /// a click is a user answer; dropping it for bookkeeping gaps is loss.
    known: bool,
    options: Vec<OptionPair>,
    multiselect: bool,
    /// Live multiselect toggle set, click order (M2b toggle-off removes).
    toggled: Vec<String>,
    /// The toggle set moved since the last freeze (Confirm/Skip): unfinished
    /// interaction that still owes an answer, distinct from the set simply
    /// mirroring what was already sent.
    toggles_dirty: bool,
    draft: Option<Draft>,
    /// An answer for this screen was already sent → further lines say 更正.
    submitted: bool,
}

impl ScreenState {
    fn from_envelope(env: &ScreenEnvelope) -> Self {
        Self {
            known: true,
            options: env
                .options
                .iter()
                .map(|opt| (opt.id.clone(), opt.label.clone()))
                .collect(),
            multiselect: env.multiselect,
            toggled: Vec::new(),
            toggles_dirty: false,
            draft: None,
            submitted: false,
        }
    }

    fn unknown() -> Self {
        Self {
            known: false,
            options: Vec::new(),
            multiselect: false,
            toggled: Vec::new(),
            toggles_dirty: false,
            draft: None,
            submitted: false,
        }
    }

    /// Display-only screens produce no messages (M2d). Unknown screens are
    /// *not* display-only — their clicks still owe an answer.
    fn is_display_only(&self) -> bool {
        self.known && self.options.is_empty()
    }

    /// A screen whose clicks commit at the boundary without a freeze click
    /// (single-select, or unknown — see `known`).
    fn is_single_semantics(&self) -> bool {
        !self.multiselect || !self.known
    }

    fn label_for(&self, option_id: &str) -> String {
        let raw = self
            .options
            .iter()
            .find(|(id, _)| id == option_id)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| option_id.to_string());
        sanitize_label(&raw)
    }
}

/// Label hygiene (M2c — defense in depth; the envelope validator already
/// rejects control chars and the message prefix can never be a slash-command):
/// newlines become spaces, a leading `/` becomes fullwidth `／`.
fn sanitize_label(raw: &str) -> String {
    let mut cleaned: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if cleaned.starts_with('/') {
        cleaned.replace_range(..1, "／");
    }
    cleaned
}

/// Click mailbox: per-screen draft state plus the two boundary drains.
///
/// The event loop feeds [`Self::push_event`] from the canvas subscription and
/// polls [`Self::take_submit`] each tick; the Enter send site calls
/// [`Self::take_for_enter`] to merge pending picks behind the typed text.
#[derive(Debug, Default)]
pub(crate) struct ClickMailbox {
    screens: BTreeMap<u64, ScreenState>,
}

impl ClickMailbox {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Queue one click (M2: never injected into an active turn — this only
    /// records). `screen` is the pushed envelope for the event's screen seq,
    /// when the host can look it up; `now` stamps the click for the coalesce
    /// window.
    pub(crate) fn push_event(
        &mut self,
        now: Instant,
        event: &CanvasEvent,
        screen: Option<&ScreenEnvelope>,
    ) {
        self.screens
            .entry(event.screen)
            .or_insert_with(|| match screen {
                Some(env) => ScreenState::from_envelope(env),
                None => ScreenState::unknown(),
            });
        let state = self
            .screens
            .get_mut(&event.screen)
            .expect("entry ensured above");

        if state.is_display_only() {
            return;
        }

        match &event.action {
            EventAction::Select { option_id } => {
                if state.is_single_semantics() {
                    state.draft = Some(Draft::Single {
                        option_id: option_id.clone(),
                        clicked_at: now,
                    });
                } else if let Some(pos) = state.toggled.iter().position(|id| id == option_id) {
                    // M2b: re-clicking a selected item cancels it.
                    state.toggled.remove(pos);
                    state.toggles_dirty = true;
                } else {
                    state.toggled.push(option_id.clone());
                    state.toggles_dirty = true;
                }
            }
            EventAction::Confirm => {
                // Server-side Confirm is invalid on single-select screens;
                // belt-and-braces, ignore rather than mis-synthesize.
                if state.is_single_semantics() {
                    return;
                }
                state.draft = Some(if state.toggled.is_empty() {
                    // Confirming an empty set is "nothing chosen" (M2b skip).
                    Draft::Skipped
                } else {
                    Draft::Confirmed {
                        ids: state.toggled.clone(),
                    }
                });
                // The freeze absorbs the set — it is only "pending" again once
                // it moves (correction material).
                state.toggles_dirty = false;
            }
            EventAction::Skip => {
                state.draft = Some(Draft::Skipped);
                state.toggled.clear();
                state.toggles_dirty = false;
            }
        }
    }

    /// Idle boundary: synthesize and consume whatever is due (M2a). Held while
    /// a root turn is in flight (`busy`) or while typed text waits for Enter
    /// (`composer_holds` — M2e merges at Enter; auto-submitting under the
    /// user's fingers would split their message). Debt is never dropped: held
    /// drafts stay and fire on a later boundary.
    pub(crate) fn take_submit(
        &mut self,
        now: Instant,
        busy: bool,
        composer_holds: bool,
    ) -> Option<String> {
        if busy || composer_holds {
            return None;
        }
        let lines = self.collect_lines(now, false);
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    /// Enter boundary (M2e): merge pending picks behind the typed text —
    /// typed first, one blank line, then the `(画布选择)` lines — and consume
    /// them. Unconfirmed multi-select toggle sets do *not* merge (M2b commits
    /// only on Confirm); they stay queued. Returns `None` when nothing is
    /// pending, so the caller submits its typed text alone.
    ///
    /// A `/`-leading submit is a slash action, not a message (M2c): never
    /// merge into it — the merge would mask the command behind pick text.
    /// The picks stay queued for the next boundary (debt not lost).
    pub(crate) fn take_for_enter(&mut self, typed: &str) -> Option<String> {
        if typed.trim_start().starts_with('/') {
            return None;
        }
        // Flush ignores the coalesce window: Enter is an explicit user action,
        // and a pick clicked a moment ago belongs with their sentence (M2e's
        // "以 Enter 为界" cuts the other way too — everything before it merges).
        let lines = self.collect_lines(Instant::now(), true);
        if lines.is_empty() {
            return None;
        }
        let joined = lines.join("\n");
        Some(if typed.trim().is_empty() {
            joined
        } else {
            format!("{}\n\n{}", typed.trim_end(), joined)
        })
    }

    /// True while user interaction owes a message: a queued draft or a toggle
    /// set that moved since its last freeze. Coarse face of the same ledger as
    /// [`Self::pending_screens`] (which screen owes what) — kept because it is
    /// the mailbox's whole-state question.
    #[allow(dead_code)] // read by tests; the run loop uses the per-screen lists
    pub(crate) fn has_pending(&self) -> bool {
        self.screens
            .values()
            .any(|state| state.draft.is_some() || state.toggles_dirty)
    }

    /// Screen seqs whose answer was already sent (1C: the waiting screen only
    /// covers settled choices; its summary lists them under 已定).
    pub(crate) fn answered_screens(&self) -> Vec<u64> {
        self.screens
            .iter()
            .filter(|(_, state)| state.submitted)
            .map(|(seq, _)| *seq)
            .collect()
    }

    /// Screen seqs that still owe an answer (draft / moved toggle set) — the
    /// status bar and the waiting trigger's `draft` fact.
    pub(crate) fn pending_screens(&self) -> Vec<u64> {
        self.screens
            .iter()
            .filter(|(_, state)| state.draft.is_some() || state.toggles_dirty)
            .map(|(seq, _)| *seq)
            .collect()
    }

    /// Screen seqs whose answer is frozen but not yet delivered — a draft
    /// sitting behind a held boundary (busy turn / composer holding for Enter).
    /// This is the busy-click ack (`已选待送`): the pick *did* land, it just
    /// cannot become a user message mid-turn (M2). Stricter than
    /// [`Self::pending_screens`] on purpose: a half-toggled multi-select is
    /// still being interacted with, not waiting to be sent.
    pub(crate) fn queued_screens(&self) -> Vec<u64> {
        self.screens
            .iter()
            .filter(|(_, state)| state.draft.is_some())
            .map(|(seq, _)| *seq)
            .collect()
    }

    /// Collect due drafts into synthesized lines and consume them (report-once,
    /// same accounting as `bg_wake`: flip `submitted` before returning so a
    /// crash between collect and send can never double-report). `flush` skips
    /// the coalesce window (Enter boundary).
    fn collect_lines(&mut self, now: Instant, flush: bool) -> Vec<String> {
        let mut lines = Vec::new();
        for state in self.screens.values_mut() {
            let due = match &state.draft {
                None => false,
                Some(Draft::Single { clicked_at, .. }) => {
                    flush || now.saturating_duration_since(*clicked_at) >= CLICK_COALESCE
                }
                Some(Draft::Confirmed { .. } | Draft::Skipped) => true,
            };
            if !due {
                continue;
            }
            let draft = state.draft.take().expect("draft checked above");
            lines.push(synthesize(state, &draft));
            state.submitted = true;
        }
        // Two versions of one question can each queue a pick: the model re-pushes
        // mid-turn and cannot see the click on the earlier screen, so the user
        // answers again. When both read the same, the user answered once — keep
        // the newest line only. A differing repeat stays as the trail (the last
        // line is the live answer).
        let mut out: Vec<String> = Vec::with_capacity(lines.len());
        for line in lines {
            if let Some(pos) = out.iter().position(|kept| *kept == line) {
                out.remove(pos);
            }
            out.push(line);
        }
        out
    }
}

/// One M2d line for a consumed draft. `更正` marks only screens that already
/// sent an answer ("提交后再点同一屏").
fn synthesize(state: &ScreenState, draft: &Draft) -> String {
    let correction = if state.submitted { "更正：" } else { "" };
    match draft {
        Draft::Single { option_id, .. } => {
            pick_line(state, std::slice::from_ref(option_id), correction)
        }
        Draft::Confirmed { ids } => pick_line(state, ids, correction),
        Draft::Skipped => format!("{ANSWER_PREFIX} {correction}跳过 · 不做选择"),
    }
}

/// `(画布选择) [更正：]已选 {ids} · {labels}` — ids in click order joined by
/// `、`, labels in the same order joined by `, ` (M2d).
fn pick_line(state: &ScreenState, ids: &[String], correction: &str) -> String {
    let labels: Vec<String> = ids.iter().map(|id| state.label_for(id)).collect();
    format!(
        "{ANSWER_PREFIX} {correction}已选 {} · {}",
        ids.join("、"),
        labels.join(", ")
    )
}

#[cfg(test)]
#[path = "click_mailbox_tests.rs"]
mod click_mailbox_tests;
