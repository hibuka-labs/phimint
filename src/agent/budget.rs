use super::*;

/// Options for token-budget context management.
///
/// When `Some`, enables window rotation + history/notes tools instead of
/// LLM-based summarization.
#[derive(Clone, Debug)]
pub struct TokenBudgetOptions {
    /// Work-room budget: conversation tokens ABOVE the window's fixed base
    /// (system prompt + boilerplate). The base is estimated by the core at
    /// build time; the window resets at base + budget + buffer.
    pub budget: usize,
    /// Session retention days for history/notes cleanup.
    pub retention_days: i64,
    /// Base directory (~/.phimint/).
    pub base_dir: PathBuf,
    /// Session ID.
    pub session_id: String,
}
