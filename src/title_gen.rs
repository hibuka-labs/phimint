//! Session title generation via LLM.
//!
//! Generates a concise Chinese title for a session based on the first N user
//! messages.  Uses the lite model (or falls back to the main model) for speed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use phi_agent::llm_trait::message::ChatMessage as LlmChatMessage;
use phi_agent::llm_trait::provider::LlmProvider;
use phi_agent::llm_trait::request::ChatRequest;
use phi_agent::{ChatMessage, load_session_messages, write_session_title};

/// Maximum number of user messages to feed into the title-generation prompt.
const TITLE_INPUT_LIMIT: usize = 10;

/// Maximum length of generated title (characters).
const MAX_TITLE_LENGTH: usize = 20;

/// Prompt sent to the LLM for title generation.
const TITLE_PROMPT: &str = "你是一个标题生成器。根据以下对话内容，生成一个简短的中文标题（不超过 20 个字）。\
只输出标题本身，不要任何解释、引号或标点符号前缀。";

/// Generate a session title from user messages and persist it to `session_meta.json`.
///
/// - `provider`: the LLM provider to use (lite model preferred).
/// - `messages`: the full conversation history (User messages are extracted internally).
/// - `session_dir`: path to the session directory.
///
/// Returns `Ok(true)` if a new title was generated and persisted,
/// `Ok(false)` if the title was already generated (`title_generated = true`),
/// or `Err` if generation failed.
pub async fn generate_session_title(
    provider: Arc<dyn LlmProvider>,
    messages: &[ChatMessage],
    session_dir: &Path,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Already generated at the threshold — nothing to do.
    if is_title_generated(session_dir) {
        return Ok(false);
    }

    let user_texts = collect_user_texts(messages);
    if user_texts.is_empty() {
        return Ok(false);
    }

    let title = call_llm_for_title(provider, &user_texts).await?;
    if title.is_empty() {
        return Ok(false);
    }

    write_session_title(session_dir, &title, true)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;

    Ok(true)
}

/// Write a pending-title marker so the title is generated on next startup.
///
/// This is a pure file-write — zero blocking, zero network.
pub fn write_pending_title_marker(sessions_dir: &Path, session_dir: &Path) {
    let pending_path = sessions_dir.join(".pending_title.json");
    let pending = serde_json::json!({
        "session_dir": session_dir.to_string_lossy(),
    });
    if let Err(e) = std::fs::write(&pending_path, serde_json::to_string_pretty(&pending).unwrap()) {
        tracing::warn!(error = %e, "failed to write pending title marker");
    }
}

/// Process a pending title generation marker left by a previous session exit.
///
/// Called at startup (before the picker). If `.pending_title.json` exists in the
/// sessions root, loads the session's messages and generates a title. The marker
/// is deleted regardless of success.
pub async fn process_pending_title(
    provider: Arc<dyn LlmProvider>,
    sessions_dir: &Path,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let pending_path = sessions_dir.join(".pending_title.json");
    if !pending_path.exists() {
        return Ok(false);
    }

    let content = match std::fs::read_to_string(&pending_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read pending title marker");
            let _ = std::fs::remove_file(&pending_path);
            return Ok(false);
        }
    };

    let pending: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "invalid pending title marker");
            let _ = std::fs::remove_file(&pending_path);
            return Ok(false);
        }
    };

    let session_dir = match pending["session_dir"].as_str() {
        Some(s) => PathBuf::from(s),
        None => {
            tracing::warn!("pending title marker missing session_dir");
            let _ = std::fs::remove_file(&pending_path);
            return Ok(false);
        }
    };

    // Always clean up the marker, even if generation fails.
    let _ = std::fs::remove_file(&pending_path);

    if !session_dir.exists() {
        return Ok(false);
    }

    let messages = load_session_messages(&session_dir)?;
    if messages.is_empty() {
        return Ok(false);
    }

    generate_session_title(provider, &messages, &session_dir).await
}

/// Collect the first N user message texts.
fn collect_user_texts(messages: &[ChatMessage]) -> Vec<&str> {
    messages
        .iter()
        .filter_map(|m| match m {
            ChatMessage::User { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .take(TITLE_INPUT_LIMIT)
        .collect()
}

/// Call the LLM to generate a title from user message texts.
async fn call_llm_for_title(
    provider: Arc<dyn LlmProvider>,
    user_texts: &[&str],
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let conversation = user_texts
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}. {}", i + 1, t))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt_content = format!("{}\n\n{}", TITLE_PROMPT, conversation);

    let request = ChatRequest::new(vec![LlmChatMessage::user(prompt_content)]);

    let response = provider.chat(request).await?;
    let title = response.content.trim().to_string();
    
    // Validate title length and content
    if title.is_empty() {
        return Ok(String::new());
    }
    
    // Truncate if too long
    let title = if title.chars().count() > MAX_TITLE_LENGTH {
        title.chars().take(MAX_TITLE_LENGTH).collect()
    } else {
        title
    };
    
    // Remove any quotes or special characters that might have been added
    let title = title.trim_matches(|c: char| c == '"' || c == ''' || c == '「' || c == '」');
    
    Ok(title.to_string())
}

/// Check whether the title has already been generated at the threshold.
fn is_title_generated(session_dir: &Path) -> bool {
    let meta_path = session_dir.join("session_meta.json");
    let Ok(content) = std::fs::read_to_string(&meta_path) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    meta["title_generated"].as_bool().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_generated_false_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_title_generated(dir.path()));
    }

    #[test]
    fn is_generated_false_when_not_set() {
        let dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({ "session_id": "test" });
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        assert!(!is_title_generated(dir.path()));
    }

    #[test]
    fn is_generated_true_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let meta = serde_json::json!({ "title": "测试", "title_generated": true });
        std::fs::write(
            dir.path().join("session_meta.json"),
            serde_json::to_string_pretty(&meta).unwrap(),
        )
        .unwrap();
        assert!(is_title_generated(dir.path()));
    }
}
