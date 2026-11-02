//! Tests for the canvas host policy (1C).
//!
//! Covers the automation rows this module owns: row 13 (status number = the
//! envelope's `options.len()`), row 14's policy half (the model can never pop
//! a browser — consent is raised once per URL and a deny never launches), and
//! row 15 (waiting-screen trigger). Plus the waiting summary's escaping and
//! the rebind reset. Browser spawning is injected, so no window ever opens.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use phi_canvas::tools::BrowserLauncher;
use phi_canvas::{CloseReason, PushReceipt};
use tokio::sync::mpsc;

use super::*;
use crate::approval::ApprovalItem;

pub(super) fn receipt(seq: u64, title: &str, options: usize, display_only: bool) -> PushReceipt {
    PushReceipt {
        screen: seq,
        url: "http://127.0.0.1:32123".to_string(),
        title: title.to_string(),
        option_count: options,
        display_only,
        interstitial: false,
    }
}

/// The host's own pause push (the waiting screen), as its receipt looks.
pub(super) fn pause(seq: u64, title: &str) -> PushReceipt {
    PushReceipt {
        interstitial: true,
        ..receipt(seq, title, 0, true)
    }
}

pub(super) fn face(options: usize, display_only: bool, waiting: bool) -> ScreenFace {
    ScreenFace {
        option_count: options,
        display_only,
        waiting,
    }
}

pub(super) fn facts() -> AnswerFacts {
    AnswerFacts::default()
}

pub(super) fn waiting_input<'a>(current: Option<&'a ScreenFace>) -> WaitingInput<'a> {
    WaitingInput {
        current,
        current_seq: Some(7),
        push_turn: 1,
        turn: 2,
        waiting_seq: None,
        answered: true,
        draft: false,
        drain_started_turn: false,
    }
}

pub(super) type LaunchCounter = Arc<AtomicUsize>;

pub(super) fn host_with_counter() -> (
    Arc<CanvasHost>,
    mpsc::UnboundedReceiver<ApprovalItem>,
    LaunchCounter,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let counter: LaunchCounter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    let host = CanvasHost::with_launch_fn(tx, move |_| {
        counter_clone.fetch_add(1, Ordering::SeqCst);
        LaunchOutcome::Spawned
    });
    (host, rx, counter)
}

pub(super) type UrlLog = Arc<std::sync::Mutex<Vec<String>>>;

pub(super) fn host_recording_urls() -> (
    Arc<CanvasHost>,
    mpsc::UnboundedReceiver<ApprovalItem>,
    UrlLog,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let urls: UrlLog = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&urls);
    let host = CanvasHost::with_launch_fn(tx, move |url| {
        recorded.lock().unwrap().push(url.to_string());
        LaunchOutcome::Spawned
    });
    (host, rx, urls)
}

pub(super) fn answer_yes(rx: &mut mpsc::UnboundedReceiver<ApprovalItem>) {
    let item = rx.try_recv().expect("consent prompt queued");
    item.decision_tx.send(ApprovalDecision::AllowOnce).unwrap();
}

pub(super) fn launched_urls(urls: &UrlLog) -> Vec<String> {
    urls.lock().unwrap().clone()
}

mod face;
mod launch;
mod lifecycle;
