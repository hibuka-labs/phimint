//! phimint library crate.
//!
//! The binary (src/main.rs) is a thin shell over these modules; keeping them
//! in a lib target lets `tests/` drive the real agent stack (mock LLM →
//! build() → turn) — the skill-tool integration test is the first consumer.

pub mod agent;
pub mod approval;
pub mod banner;
pub mod bin_shadow;
pub mod browser_face;
pub mod config;
pub mod context_rotation;
// The desktop capability pack (desk.*) on the code-exec engine — compiled out
// with the engine under `--no-default-features`.
#[cfg(feature = "code-exec")]
pub mod desk;
// The device consent posture (posture B) and the bridge to the approval
// popup — same feature gate: no engine, no consent seam.
#[cfg(feature = "code-exec")]
pub mod desk_consent;
pub mod external;
pub mod file_face;
pub mod interact_slot;
pub mod interaction;
pub mod model_store;
pub mod router;
pub mod session_birthplace;
pub mod session_history;
pub mod session_scope;
pub mod skills;
pub mod terminal_restore;
pub mod tips;
pub mod title_gen;
pub mod tools;
pub mod ui;
pub mod uninstall;
pub mod update;
