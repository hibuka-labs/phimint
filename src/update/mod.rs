//! Update module: version checking and self-update for phimint.
//!
//! Provides manifest parsing, platform detection, endpoint fallback checking,
//! install-source detection (upgrade routing) and state persistence. The
//! upgrade hint is surfaced to the TUI via `TuiEvent::UpdateAvailable`;
//! `apply` implements `phimint update` for standalone installs.

pub mod apply;
pub mod checker;
pub mod error;
pub mod install_source;
pub mod manifest;
pub mod platform;
pub mod state;
