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
//! and message synthesis (M2) live in `ui::click_mailbox`.
//!
//! `ask_user` is the text half of that routing: a concept question answered
//! in a terminal picker rather than on the canvas.

pub mod add_tool;
pub mod ask_user;
pub mod browser;
pub mod canvas;
pub mod diagnostics;
pub mod repomap;
pub mod ripgrep;
