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

pub mod add_tool;
pub mod diagnostics;
pub mod repomap;
pub mod ripgrep;
