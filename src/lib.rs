//! phimint library crate.
//!
//! The binary (src/main.rs) is a thin shell over these modules; keeping them
//! in a lib target lets `tests/` drive the real agent stack (mock LLM →
//! build() → turn) — the skill-tool integration test is the first consumer.

pub mod agent;
pub mod approval;
pub mod banner;
pub mod cli;
pub mod command;
pub mod external;
pub mod model;
pub mod ops;
pub mod session;
pub mod shell;
pub mod tips;
pub mod tools;
pub mod update;
pub mod verify_ledger;

// The desktop capability pack (desk.*) on the code-exec engine — compiled out
// with the engine under `--no-default-features`. It carries the device consent
// posture (posture B) and the bridge to the approval popup: no engine, no
// consent seam.
#[cfg(feature = "code-exec")]
pub mod desk;
