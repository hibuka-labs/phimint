//! Canvas host policy (1C): what the TUI shows and decides about the design
//! canvas — the status-bar face, the `y` consent gate, the waiting-screen
//! trigger, and reopen bookkeeping.
//!
//! Decision functions are pure and sit at the top (unit-tested without a
//! browser or a TUI); [`CanvasHost`] is the thin bookkeeper that feeds them.
//! Side effects stay at the edges: `run.rs` pushes screens and redraws, and
//! the one spawn site is `tools::browser::launch_browser` behind an
//! injectable launch fn.
//!
//! Consent is deliberately *not* a tool approval (M3 + negative list #1):
//! every model-triggered launch — `canvas_open`, or an implicit `canvas_push`
//! — raises the prompt, in every approval mode, once per canvas URL. Only the
//! user's own Ctrl+B is its own consent. `allow always` is mapped to
//! launch-once: standing consent would let a later implicit push pop a browser
//! without the user's yes.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use phi_agent::{ApprovalDecision, ApprovalRequest, RiskLevel};
use phi_canvas::assets::escape_html;
use phi_canvas::{LaunchDecision, PushReceipt, ScreenEnvelope};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::click_mailbox::ANSWER_PREFIX;
use crate::approval::ApprovalItem;
use crate::tools::browser::{LaunchOutcome, launch_browser};

/// The `raw` payload key that marks a consent prompt as a canvas launch (the
/// approval renderer switches its key legend on this, never on the title).
pub const CANVAS_LAUNCH_KEY: &str = "canvas_launch";

/// Which screens the mailbox has settled. `answered` = an answer was sent;
/// `pending` = one is still owed (draft / moved toggle set); `queued` = the
/// subset of `pending` whose answer is frozen and only waits for a boundary
/// (the busy-click ack — see `click_mailbox::queued_screens`). Screen seqs.
/// `queued_labels` names those frozen picks (see
/// `click_mailbox::queued_labels`) for the status bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerFacts {
    pub answered: BTreeSet<u64>,
    pub pending: BTreeSet<u64>,
    pub queued: BTreeSet<u64>,
    pub queued_labels: Vec<String>,
}

/// What the current screen is, as the status bar needs it (row 13: the number
/// is the envelope's `options.len()` — never scraped HTML).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenFace {
    pub option_count: usize,
    pub display_only: bool,
    /// The host's own waiting screen (its summary is not a choice).
    pub waiting: bool,
}

/// Status-bar face of the canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenStatus {
    /// `options: []` — a mockup / comparison to look at.
    DisplayOnly,
    /// `n` options awaiting the user's pick (nothing consumed, or a fresh
    /// draft after a correction).
    Awaiting { options: usize },
    /// The host's waiting screen ("continue in the terminal").
    Waiting,
    /// Answer sent and nothing new, or nothing pushed yet.
    Idle,
}

/// Screen face from the current record + mailbox facts.
pub fn screen_status(face: Option<&ScreenFace>, answered: bool, draft: bool) -> ScreenStatus {
    let Some(face) = face else {
        return ScreenStatus::Idle;
    };
    if face.waiting {
        return ScreenStatus::Waiting;
    }
    if face.display_only || face.option_count == 0 {
        return ScreenStatus::DisplayOnly;
    }
    if answered && !draft {
        ScreenStatus::Idle
    } else {
        ScreenStatus::Awaiting {
            options: face.option_count,
        }
    }
}

/// Inputs to [`status_line`] — the lifecycle phase plus the screen face, and
/// the mailbox's frozen-undelivered count and labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInput {
    pub port: u16,
    /// The user has not answered whether a browser may open (待同意).
    pub awaiting_consent: bool,
    /// Server task died without a lifecycle close (row 16).
    pub failed: bool,
    /// Graceful close (`canvas_close` / idle auto-close / session end).
    pub closed: bool,
    pub screen: ScreenStatus,
    /// Picks frozen in the mailbox, waiting for a boundary (see
    /// [`AnswerFacts::queued`]). When non-zero the ack face wins — "did my
    /// click land?" outranks what the screen is showing.
    pub queued: usize,
    /// What those picks *are* (`AnswerFacts::queued_labels`) — the ack names
    /// them so "which one did I click?" needs no trip back to the browser.
    pub queued_labels: Vec<String>,
}

/// The status-bar line: spec-locked wording, plus the alert flag (red + the
/// reopen key) for the crash face.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub text: String,
    pub alert: bool,
}

/// One status-bar-worth of pick labels: joined in click order, then cut to a
/// display budget so a long CJK label cannot shove the rest of the bar off.
fn fit_labels(labels: &[String]) -> String {
    const BUDGET: usize = 20;
    let joined = labels.join("、");
    if unicode_width::UnicodeWidthStr::width(joined.as_str()) <= BUDGET {
        return joined;
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in joined.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        // Room for the ellipsis that ends the cut.
        if used + width + 1 > BUDGET {
            out.push('…');
            return out;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// The status-bar line (spec-locked state words after `canvas ● localhost:`).
pub fn status_line(input: &StatusInput) -> StatusLine {
    if input.failed {
        return StatusLine {
            text: "canvas ● 已断开 (ctrl+b 重开)".to_string(),
            alert: true,
        };
    }
    if input.closed {
        return StatusLine {
            text: "画布已关闭 · 可重开".to_string(),
            alert: false,
        };
    }
    let state = if input.awaiting_consent {
        "待同意".to_string()
    } else if input.queued > 0 {
        // Busy-click ack: the pick is recorded, the boundary (turn end /
        // Enter) will send it. Without this face a click during a long turn
        // looks like nothing happened (walkthrough 烦②). The labels answer
        // the follow-up question the count alone raises — which one?
        let labels = fit_labels(&input.queued_labels);
        if labels.is_empty() {
            format!("{} 个已选待送", input.queued)
        } else if input.queued == 1 {
            format!("已选待送：{labels}")
        } else {
            format!("{} 个已选待送：{labels}", input.queued)
        }
    } else {
        match input.screen {
            ScreenStatus::Awaiting { options } => format!("{options} 个选项待选"),
            ScreenStatus::DisplayOnly => "展示中".to_string(),
            ScreenStatus::Waiting | ScreenStatus::Idle => "空闲".to_string(),
        }
    };
    StatusLine {
        text: format!("canvas ● localhost:{} · {state}", input.port),
        alert: false,
    }
}

/// Inputs to [`should_push_waiting`] (row 15).
#[derive(Debug, Clone, Copy)]
pub struct WaitingInput<'a> {
    /// The screen the browser shows now.
    pub current: Option<&'a ScreenFace>,
    /// Its seq — for the "already waiting" and freshness checks.
    pub current_seq: Option<u64>,
    /// Turn the current screen was pushed in (turns are labelled by the end
    /// they settle at).
    pub push_turn: u64,
    /// The turn that just ended.
    pub turn: u64,
    /// Seq of the host's own waiting screen, if one is up.
    pub waiting_seq: Option<u64>,
    /// Mailbox: the current screen already sent its answer.
    pub answered: bool,
    /// Mailbox: the current screen still owes one (draft / moved toggles).
    pub draft: bool,
    /// The boundary drain started a new user turn (a click was consumed).
    pub drain_started_turn: bool,
}

/// 1C row 15 — the waiting screen may only replace a screen that is *done*:
///
/// - never when the boundary drain just started a user turn (the agent is
///   about to answer the pick — covering it now is exactly the "抹掉刚选的屏"
///   the spec forbids);
/// - never over the waiting screen itself (no churn);
/// - never over a screen that landed during the turn that just ended (the
///   agent was just showing it — a fresh mockup must survive its own turn);
/// - for a choice screen, only once its answer was consumed and no new draft
///   is owed; a look-only screen is always done being looked at.
pub fn should_push_waiting(input: &WaitingInput<'_>) -> bool {
    if input.drain_started_turn {
        return false;
    }
    let Some(face) = input.current else {
        return false;
    };
    if input
        .current_seq
        .is_some_and(|seq| Some(seq) == input.waiting_seq)
    {
        return false;
    }
    if input.push_turn == input.turn {
        return false;
    }
    if face.display_only || face.option_count == 0 {
        return true;
    }
    input.answered && !input.draft
}

/// The host's waiting screen (spec-locked): one fixed line plus the
/// decided/pending summary. Every fragment of user-supplied text is
/// HTML-escaped (labels come from the model, so do titles).
///
/// It is interstitial, not a design version: it gets a seq and a page like
/// any other screen, but it stays out of the canvas version rail and the
/// compare pickers so the history the user scrolls is the design.
pub fn waiting_envelope(decided: &[String], pending: &[String]) -> ScreenEnvelope {
    let mut body = String::new();
    let section = |heading: &str, items: &[String], body: &mut String| {
        if items.is_empty() {
            return;
        }
        body.push_str(&format!("<h3>{heading}</h3><ul>"));
        for item in items {
            body.push_str(&format!("<li>{}</li>", escape_html(item)));
        }
        body.push_str("</ul>");
    };
    section("已定", decided, &mut body);
    section("待定", pending, &mut body);
    ScreenEnvelope {
        title: "继续在终端讨论…".to_string(),
        subtitle: None,
        options: Vec::new(),
        body_html: body,
        screen: None,
        multiselect: false,
        reference: None,
        escape: None,
        interstitial: true,
    }
}

/// The `(画布选择)` lines inside a synthesized submit (they may be merged
/// behind typed text — M2e). The waiting summary keeps them for 已定.
pub fn canvas_answer_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.starts_with(ANSWER_PREFIX))
        .map(str::to_string)
        .collect()
}

/// Is this approval prompt a canvas-launch consent (not a tool approval)?
pub fn is_canvas_consent(request: &ApprovalRequest) -> bool {
    request
        .raw
        .as_ref()
        .is_some_and(|raw| raw.get(CANVAS_LAUNCH_KEY).is_some())
}

fn consent_request(url: &str) -> ApprovalRequest {
    ApprovalRequest {
        title: "Open the design canvas?".to_string(),
        message: format!(
            "The agent prepared a design canvas at {url}.\n\n\
             y opens it in your browser — nothing opens without your yes.\n\
             n keeps the URL here (it is printed in the transcript); the canvas\n\
             itself keeps working either way."
        ),
        action_key: None,
        risk_level: RiskLevel::Safe,
        raw: Some(json!({ CANVAS_LAUNCH_KEY: { "url": url } })),
        source: None,
    }
}

/// Consent bookkeeping for one canvas URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Consent {
    /// Nobody has asked yet (a push/open is about to).
    NotRaised,
    /// Prompt is on the queue — status reads 待同意.
    Prompted,
    /// The user said yes (or Ctrl+B'd) and the URL was already shown — a
    /// later model request reports `browser launched` without re-opening.
    Granted,
    /// The user said no, or the gate settled without a launch — never nag
    /// again for this canvas.
    Declined,
}

/// What a consent answer means for the run loop (M3 + row 16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// Cache this as the tool layer's launch state (M3 text honesty).
    Decision(LaunchDecision),
    /// The user said yes to a canvas that has since closed or crashed — there
    /// is nothing live to open. Recover the way Ctrl+B does: rebind a fresh
    /// server and open *that*. The gate promised "y opens it in your browser";
    /// a dead page is plumbing, not a detour the user should route around.
    ReopenAndLaunch,
}

/// One pushed screen, as the status bar and the waiting summary need it.
#[derive(Debug, Clone)]
struct ScreenRecord {
    seq: u64,
    title: String,
    option_count: usize,
    display_only: bool,
    /// Turn the screen landed in (turns are labelled by their end).
    push_turn: u64,
}

#[derive(Debug)]
struct Inner {
    phase: Phase,
    url: Option<String>,
    port: u16,
    consent: Consent,
    /// Every push seen (tool and host alike), newest last. Idempotent by seq.
    screens: Vec<ScreenRecord>,
    /// Seq of the host's own waiting screen, if one is up.
    waiting_seq: Option<u64>,
    /// Answer lines already sent — the summary's 已定 list (capped).
    decided: Vec<String>,
    /// Turn label: incremented at each root-turn end, so `push_turn == turn`
    /// means "landed during the turn that is ending".
    turn: u64,
    /// A one-shot consent prompt, with the channel the UI answers on.
    pending: Option<oneshot::Receiver<ApprovalDecision>>,
    pending_url: Option<String>,
    /// Transcript notes the run loop should surface (headless / denied URL).
    notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No canvas in this session yet — nothing to show.
    None,
    /// Bound and serving.
    Live,
    /// Graceful close (`canvas_close` / idle / session end) — 可重开.
    Closed,
    /// Server task died without a close (row 16).
    Failed,
}

/// Cap for the waiting summary lists — a session can accumulate a long tail.
const SUMMARY_CAP: usize = 5;

/// Canvas host: the bookkeeper behind the pure policy above. Shared between
/// the run loop (status, turn ends) and the tool side (launch requests).
pub struct CanvasHost {
    inner: Mutex<Inner>,
    prompts_tx: mpsc::UnboundedSender<ApprovalItem>,
    /// The single spawn site (injectable for tests: never opens a browser).
    launch: Box<dyn Fn(&str) -> LaunchOutcome + Send + Sync>,
}

impl CanvasHost {
    /// Real host: prompts flow to the TUI's approval queue, launches go
    /// through [`launch_browser`].
    pub fn new(prompts_tx: mpsc::UnboundedSender<ApprovalItem>) -> Arc<Self> {
        Self::with_launch_fn(prompts_tx, launch_browser)
    }

    /// Test seam: same policy, a launch fn that records instead of spawning.
    pub fn with_launch_fn(
        prompts_tx: mpsc::UnboundedSender<ApprovalItem>,
        launch: impl Fn(&str) -> LaunchOutcome + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                phase: Phase::None,
                url: None,
                port: 0,
                consent: Consent::NotRaised,
                screens: Vec::new(),
                waiting_seq: None,
                decided: Vec::new(),
                turn: 0,
                pending: None,
                pending_url: None,
                notes: Vec::new(),
            }),
            prompts_tx,
            launch: Box::new(launch),
        })
    }

    /// The live canvas (re)bound: a new URL starts a fresh consent question
    /// and a fresh screen history. `decided` is kept — those picks happened.
    ///
    /// A prompt already raised for the *new* URL survives the note (the tool's
    /// `canvas_open` can ask before the run loop's tick notices the bind);
    /// resetting it there would ask twice. A prompt for any other URL is stale
    /// and is dropped with its pending receiver.
    pub fn note_canvas(&self, url: &str, port: u16) {
        let mut inner = self.inner.lock().unwrap();
        if inner.url.as_deref() != Some(url) {
            // The address moved — a rebind after a close/crash is a new port,
            // and the consent prompt still on screen (if any) named the old
            // one. Say where the design lives now.
            if inner.url.is_some() {
                inner
                    .notes
                    .push(format!("Design canvas URL (reopened): {url}"));
            }
            inner.url = Some(url.to_string());
            if inner.pending_url.as_deref() != Some(url) {
                inner.consent = Consent::NotRaised;
                inner.pending = None;
                inner.pending_url = None;
            }
            inner.screens.clear();
            inner.waiting_seq = None;
        }
        inner.port = port;
        inner.phase = Phase::Live;
    }

    /// Server task died without a lifecycle close (row 16's visible change).
    pub fn note_failed(&self) {
        self.inner.lock().unwrap().phase = Phase::Failed;
    }

    /// Graceful close / session switch (row 18) — 可重开, never auto-reopened.
    pub fn note_closed(&self) {
        self.inner.lock().unwrap().phase = Phase::Closed;
    }

    /// Record one push. Idempotent by seq: the run loop's history replay may
    /// re-offer a receipt it already recorded directly (its own waiting push).
    pub fn on_push(&self, receipt: &PushReceipt) {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .screens
            .iter()
            .any(|record| record.seq == receipt.screen)
        {
            return;
        }
        let turn = inner.turn;
        inner.screens.push(ScreenRecord {
            seq: receipt.screen,
            title: receipt.title.clone(),
            option_count: receipt.option_count,
            display_only: receipt.display_only,
            push_turn: turn,
        });
    }

    /// The host pushed its waiting screen — tag it (status face + no re-push).
    pub fn on_waiting_push(&self, seq: u64) {
        self.inner.lock().unwrap().waiting_seq = Some(seq);
    }

    /// An answer was synthesized and sent (`(画布选择)` lines, prefix kept —
    /// the summary renders them as the transcript does).
    pub fn on_answered(&self, lines: &[String]) {
        let mut inner = self.inner.lock().unwrap();
        for line in lines {
            let item = line
                .strip_prefix(ANSWER_PREFIX)
                .unwrap_or(line)
                .trim_start()
                .to_string();
            inner.decided.push(item);
        }
        let overflow = inner.decided.len().saturating_sub(SUMMARY_CAP);
        if overflow > 0 {
            inner.decided.drain(..overflow);
        }
    }

    /// The root turn settled (falling edge). Returns the waiting screen to
    /// push when row 15's conditions hold. Always advances the turn label.
    pub fn on_root_turn_end(
        &self,
        facts: &AnswerFacts,
        drain_started_turn: bool,
    ) -> Option<ScreenEnvelope> {
        let mut inner = self.inner.lock().unwrap();
        let current = inner.screens.last().map(|record| ScreenRecordLite {
            seq: record.seq,
            face: ScreenFace {
                option_count: record.option_count,
                display_only: record.display_only,
                waiting: Some(record.seq) == inner.waiting_seq,
            },
            push_turn: record.push_turn,
        });
        let (current_seq, current_face, push_turn) = match &current {
            Some(lite) => (Some(lite.seq), Some(lite.face), lite.push_turn),
            None => (None, None, 0),
        };
        let input = WaitingInput {
            current: current_face.as_ref(),
            current_seq,
            push_turn,
            turn: inner.turn,
            waiting_seq: inner.waiting_seq,
            answered: current_seq.is_some_and(|seq| facts.answered.contains(&seq)),
            draft: current_seq.is_some_and(|seq| facts.pending.contains(&seq)),
            drain_started_turn,
        };
        let push = should_push_waiting(&input);
        inner.turn += 1;
        if !push {
            return None;
        }
        let mut decided = inner.decided.clone();
        decided.reverse(); // newest pick first
        let answered = &facts.answered;
        let mut pending: Vec<String> = inner
            .screens
            .iter()
            .rev()
            .filter(|record| {
                record.option_count > 0
                    && !record.display_only
                    && Some(record.seq) != inner.waiting_seq
                    && !answered.contains(&record.seq)
            })
            .map(|record| format!("{} · {} 个选项", record.title, record.option_count))
            .take(SUMMARY_CAP)
            .collect();
        pending.reverse();
        Some(waiting_envelope(&decided, &pending))
    }

    /// The status-bar face for this tick (`None` until a canvas exists).
    pub fn status_line(&self, facts: &AnswerFacts) -> Option<StatusLine> {
        let inner = self.inner.lock().unwrap();
        if inner.phase == Phase::None {
            return None;
        }
        let record = inner.screens.last();
        let face = record.map(|record| ScreenFace {
            option_count: record.option_count,
            display_only: record.display_only,
            waiting: Some(record.seq) == inner.waiting_seq,
        });
        let answered = record.is_some_and(|r| facts.answered.contains(&r.seq));
        let draft = record.is_some_and(|r| facts.pending.contains(&r.seq));
        // The launch question is still open (nobody asked yet, or the prompt
        // is up) — the screen face does not matter until a browser can show it.
        let awaiting_consent = matches!(inner.consent, Consent::NotRaised | Consent::Prompted);
        Some(status_line(&StatusInput {
            port: inner.port,
            awaiting_consent,
            failed: inner.phase == Phase::Failed,
            closed: inner.phase == Phase::Closed,
            screen: screen_status(face.as_ref(), answered, draft),
            queued: facts.queued.len(),
            queued_labels: facts.queued_labels.clone(),
        }))
    }

    /// Model-triggered launch (M3): `canvas_open` or an implicit push. The
    /// only path that ever raises the consent prompt — idempotent per URL.
    pub fn request_model_launch(&self, url: &str) -> LaunchDecision {
        let mut inner = self.inner.lock().unwrap();
        match inner.consent {
            // The URL is already on screen (Ctrl+B, or an earlier yes) — the
            // tool may report it; never pop a second browser tab.
            Consent::Granted => LaunchDecision::Launched,
            Consent::NotRaised => {
                inner.consent = Consent::Prompted;
                let (decision_tx, decision_rx) = oneshot::channel();
                let sent = self
                    .prompts_tx
                    .send(ApprovalItem {
                        request: consent_request(url),
                        decision_tx,
                    })
                    .is_ok();
                if sent {
                    inner.pending = Some(decision_rx);
                    inner.pending_url = Some(url.to_string());
                }
                drop(inner);
                LaunchDecision::AwaitingConsent
            }
            // Prompt already up, or the user said no: never nag again.
            Consent::Prompted | Consent::Declined => LaunchDecision::AwaitingConsent,
        }
    }

    /// User-triggered launch (Ctrl+B): the keypress itself is the consent —
    /// a model never reaches this path — so the browser opens directly. This
    /// is also the recovery path (rebind + relaunch on a new port).
    pub fn request_user_launch(&self, url: &str) {
        self.inner.lock().unwrap().consent = Consent::Granted;
        self.spawn_launch(url);
    }

    /// Resolve any consent prompt the user has answered (poll seam — keeps
    /// this module free of spawned tasks and fully testable). Returns what the
    /// run loop should do with the answer: cache the tool layer's launch state
    /// once the gate has spoken (so a later `canvas_open` on the same live
    /// canvas cannot report a stale `awaiting consent` — M3 honesty), or
    /// recover a dead canvas on a yes.
    pub fn poll_consent(&self) -> Option<ConsentOutcome> {
        let (decision, url) = {
            let mut inner = self.inner.lock().unwrap();
            let result = match inner.pending.as_mut()?.try_recv() {
                Ok(decision) => Ok(decision),
                Err(oneshot::error::TryRecvError::Closed) => Err(()),
                Err(oneshot::error::TryRecvError::Empty) => return None,
            };
            let url = inner.pending_url.take().unwrap_or_default();
            inner.pending = None;
            (result, url)
        };
        match decision {
            // `allow always` maps to launch-once on purpose (negative list #1):
            // a standing consent would pop browsers without a yes later.
            Ok(ApprovalDecision::AllowOnce) | Ok(ApprovalDecision::AllowAlways) => {
                {
                    let mut inner = self.inner.lock().unwrap();
                    // The prompt can outlive the server — row 16's crash face
                    // is exactly "it died while the consent was still up". A
                    // yes is a yes to *seeing the design*, so the run loop
                    // recovers like Ctrl+B (rebind + open the new URL) instead
                    // of opening a page that cannot load. Until that happens
                    // the gate stays settled: no re-nag, no `browser launched`.
                    if matches!(inner.phase, Phase::Failed | Phase::Closed) {
                        inner.consent = Consent::Declined;
                        drop(inner);
                        return Some(ConsentOutcome::ReopenAndLaunch);
                    }
                    inner.consent = Consent::Granted;
                }
                self.spawn_launch(&url);
                Some(ConsentOutcome::Decision(LaunchDecision::Launched))
            }
            Ok(ApprovalDecision::Deny) | Err(()) => {
                self.inner.lock().unwrap().consent = Consent::Declined;
                self.push_note(format!("Design canvas URL (nothing was opened): {url}"));
                Some(ConsentOutcome::Decision(LaunchDecision::AwaitingConsent))
            }
        }
    }

    /// A transcript note the run loop has not surfaced yet.
    pub fn take_note(&self) -> Option<String> {
        self.inner.lock().unwrap().notes.pop()
    }

    fn push_note(&self, note: String) {
        self.inner.lock().unwrap().notes.push(note);
    }

    fn spawn_launch(&self, url: &str) {
        let outcome = (self.launch)(url);
        if outcome == LaunchOutcome::ShowUrl {
            // Headless: no opener worked — show the URL instead (M3).
            self.push_note(format!("Design canvas URL (no browser found): {url}"));
        }
    }
}

/// Minimal screen facts for the waiting decision inside [`CanvasHost::on_root_turn_end`].
struct ScreenRecordLite {
    seq: u64,
    face: ScreenFace,
    push_turn: u64,
}

/// The single model-facing launch port (M3): every tool-side open routes here,
/// so a browser can never appear without the user's yes. The user's Ctrl+B
/// bypasses it by design — that path *is* the user's hand.
pub struct ConsentLauncher(pub Arc<CanvasHost>);

impl phi_canvas::BrowserLauncher for ConsentLauncher {
    fn launch(&self, url: &str) -> LaunchDecision {
        self.0.request_model_launch(url)
    }
}

#[cfg(test)]
#[path = "canvas_host_tests.rs"]
mod canvas_host_tests;
