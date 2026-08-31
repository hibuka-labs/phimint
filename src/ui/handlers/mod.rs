//! Event handlers for the TUI application.
//!
//! This module contains the event handling logic split from `app.rs`:
//! - `runtime`: handles `RuntimeEvent` from the agent
//! - `keyboard`: handles keyboard input events
//! - `mouse`: handles mouse events (future)

pub mod runtime;
pub mod keyboard;
