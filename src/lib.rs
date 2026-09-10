//! phimint library crate.
//!
//! The binary (src/main.rs) is a thin shell over these modules; keeping them
//! in a lib target lets `tests/` drive the real agent stack (mock LLM →
//! build() → turn) — the skill-tool integration test is the first consumer.

pub mod agent;
pub mod approval;
pub mod banner;
pub mod context_rotation;
pub mod gate;
pub mod skills;
pub mod tools;
pub mod ui;
