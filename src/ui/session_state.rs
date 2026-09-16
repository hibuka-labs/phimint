//! Session state management for the TUI.
//!
//! [`CurrentSession`] bundles all session-related state so that `/resume`
//! (or any future session-switching command) can swap sessions by replacing
//! a single value.

use phi_agent::SessionContext;

/// The active session state for the TUI.
///
/// Holds everything that changes when the user switches sessions. Created once
/// at startup; replaced wholesale on `/resume`.
pub struct CurrentSession {
    /// Agent-base runtime session ID.
    pub session_id: phi_agent::SessionId,
    /// phi-agent session context (directory, lock, log path).
    pub ctx: SessionContext,
    /// Number of user messages sent in this TUI session (for title generation).
    pub user_msg_count: usize,
}

impl CurrentSession {
    /// Shortcut to the session directory.
    pub fn session_dir(&self) -> &std::path::Path {
        &self.ctx.session_dir
    }
}
