//! ratatui TUI (Phase 5): an independent `RuntimeEvent` consumer.
//!
//! Declarations only — implementation lives in the child modules:
//! - `app`: root state (`App`, status/task-panel types, composer state)
//! - `task_panel`: sub-agent lifecycle + per-child streaming (split impl)
//! - `render`: frame layout and drawing
//! - `handlers`: `RuntimeEvent` / keyboard / mouse input handling
//! - `child_results`: fan-in result routing decisions (unit-tested)
//! - `frame_log`: session logs (frames.txt / perf.log / composer.log)
//! - `run`: the main loop — terminal setup, channels, agent-side turn runner

// Chat-TUI component layer (lines / transcript / stream / wrap / viewport /
// selection / picker / completer / mention / input / diff / markdown) moved
// to the `phi-tui` crate (path dependency). This module keeps only the
// product shell: app state root, layout renderer, input/event handlers, and
// the TUI main loop.
pub mod app;
mod child_results;
pub mod frame_log;
pub mod handlers;
pub mod render;
mod run;
mod task_panel;

pub use run::run_tui;
