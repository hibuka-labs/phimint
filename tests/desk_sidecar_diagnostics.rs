//! The sidecar transport's diagnostics must not reach stderr.
//!
//! The transport (phi-wire-client) forwards provider diagnostics to **stderr**
//! unless the host names a log file — and in a TUI stderr is the alternate
//! screen: the text lands on the user's display and the renderer never
//! repaints those cells, so the input line grows garbage that no redraw
//! clears. The host names the file at startup (`main.rs`); this test pins
//! the other half — that the transport honors the name.

#![cfg(all(unix, feature = "code-exec"))]

use std::collections::BTreeMap;

/// The variable the transport reads (phi-wire-client's host contract).
const PROVIDER_LOG_ENV: &str = "MAI_UI_PROVIDER_LOG";

#[tokio::test]
async fn provider_diagnostics_land_in_the_named_file_not_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("provider.log");
    // SAFETY: the current-thread runtime has no worker thread, so nothing
    // else reads the environment while this runs.
    unsafe { std::env::set_var(PROVIDER_LOG_ENV, &log) };

    // A process that exits at once. The transport logs its start line
    // before the handshake, so the spawn's own diagnostics are enough to
    // prove where they went.
    let _ = phi_ns::remote::ProcessTransport::spawn(
        "/bin/sh",
        &["-c".to_string(), "exit 0".to_string()],
        &BTreeMap::new(),
    );

    let written = std::fs::read_to_string(&log)
        .expect("the named file receives the diagnostics instead of stderr");
    assert!(
        written.contains("started:"),
        "the transport logged its start line to the named file: {written}"
    );
}
