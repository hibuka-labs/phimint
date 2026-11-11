//! phimint application tools: content search, repository map, and diagnostics.
//!
//! `search_content` and `repo_map` are "pull-based" context providers (design
//! §3): rather than pushing the whole repository into the LLM, the agent asks
//! for a structural map or locates symbols by content on demand.
//!
//! The cores (search / repo map / diagnostics formatting, language registry,
//! LSP client) live in the `code-intel` crate; these modules are thin
//! `phi_agent::Tool` adapter shells over them.
//!
//! `add_tool` is the tool-socket agent-write entry (human-confirmed): it is
//! not a context provider but the agent's way to mount a script as a tool.
//!
//! `canvas` adapts the phi-canvas design-canvas tools (M3); the click mailbox
//! and message synthesis (M2) live in `crate::shell::click_mailbox`.
//!
//! `ask_user` is the text half of that routing: a concept question answered
//! in a terminal picker rather than on the canvas.
//!
//! `verify` is the compile-level check for the verify gate (design §8.3,
//! revised in `notes/verify-gate-design.md`): it runs the project's build
//! command and reports the compiler's **real exit code**. Unlike the other
//! modules here its core does not live in `code-intel` — spawning and process
//! groups are I/O, and `code-intel` stays synchronous and framework-free.
//! Parsing is the only shared part and lives in `code_intel::verify`.

pub mod add_tool;
pub mod ask_user;
pub mod browser;
pub mod canvas;
pub mod current_model;
pub mod diagnostics;
pub mod repomap;
pub mod ripgrep;
pub mod verify;
