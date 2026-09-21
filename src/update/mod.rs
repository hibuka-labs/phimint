//! Update module: version checking for phimint.
//!
//! Provides manifest parsing, platform detection, endpoint fallback checking,
//! and state persistence. The upgrade hint is surfaced to the TUI via
//! `TuiEvent::UpdateAvailable`.
//!
//! Design: see `~/.gstack/projects/tools-phimint/kangzengchen-main-design-20260920-223209.md`

pub mod checker;
pub mod error;
pub mod manifest;
pub mod platform;
pub mod state;