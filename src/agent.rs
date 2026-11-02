//! Build the phimint coding agent: system prompt + tool registration.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
// Only the code-exec assembly block calls `.context()` (M3 T10d ②); keep the
// trait import honest when the feature compiles the block out.
#[cfg(feature = "code-exec")]
use anyhow::Context;
use phi_agent::{
    ApprovalHandler, ChildPermissionMode, ControlConfig, DefaultGuard, DefaultGuardConfig,
    LocalShellTool, MaxTurnsNudgeConfig, MaxTurnsNudgeMiddleware, MemoryConfig, MultiAgentConfig,
    PhiAgent, PhiAgentConfig, ReasoningEffort, ReasoningOnlyAction, RepeatToolLimitConfig,
    RepeatToolLimitMiddleware, SessionId, SessionIdGenerator, TokenBudgetConfig, TokenBudgetCore,
    ToolPolicy, base_agent_builder_no_compression, base_agent_builder_with_excludes,
    shared_session_ids,
};
// Only the code-cell factory maps engine errors into tool errors (M2).
#[cfg(feature = "code-exec")]
use phi_agent::AgentError;

use crate::model::config::CodeExecMode;
use crate::shell::context_rotation::TokenBudgetCompactor;
use crate::shell::skills::{
    ActiveSkillSet, SkillResolver, SkillTelemetry, SkillTool, render_catalog,
};
use crate::tools::diagnostics::DiagnosticsTool;
use crate::tools::{repomap::RepoMapTool, ripgrep::RipgrepTool};
use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};
use phi_kernel_tools::background_shell::{BackgroundTaskRegistry, TaskCancelTool, TaskOutputTool};
use phi_kernel_tools::context_rotation::{
    HistoryStore, NotesStore, create_history_tools, create_notes_tools,
};
use phi_tools::{BrowserToolset, LaunchOptions as BrowserLaunchOptions};

// The code-exec engine's assembly vocabulary (M3 T10d) — one resident engine
// per runtime, mounted as either the `code_cell` tool or the CodeAct backend.
#[cfg(feature = "code-exec")]
use crate::desk::consent::{ConsentBridge, ConsentPolicy};
#[cfg(feature = "code-exec")]
use phi_agent::code_act::{CodeActExecutor, ExecCtxBridge};
#[cfg(feature = "code-exec")]
use phi_agent::code_cell::{
    Capability, ClassPolicy, CodeCellExecutor, CodeCellTool, ExecutorBuilder, InteractionBridge,
    JournalHandle, Limits, PhiNsShell, PolicyDecision, PythonWorker, SessionFactory, SessionFuture,
};

mod budget;
mod build;
mod code_road;
mod prompt;
mod subagents;

pub use budget::TokenBudgetOptions;
pub use build::{BuildOutput, build, build_with_code_exec};
pub use prompt::compose_system_prompt;

#[cfg(test)]
mod tests;
