//! Runtime auth failures (401/403) as one TUI system-line notice.
//!
//! Decision 2 of notes/install-onboarding-design.md: when the endpoint
//! rejects a request, the user must learn WHICH file to open, WHAT is in it
//! (masked) and HOW to fix it — as a system line in the transcript, never a
//! popup and never a floating layer over the composer.
//!
//! Two halves:
//!
//!   * [`watch`] wraps a provider so a rejection is seen on the way out;
//!   * [`take_notice`] hands the shell the one line to push.
//!
//! The state is process-global, like `shell::context_rotation`'s counters:
//! the boot provider, the tier providers the store builds later and the
//! shell's notice probe all have to reach the same watch, and threading an
//! `Arc` through `ModelStore::new` (a dozen call sites) would buy nothing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use phi_agent::llm_trait::{
    Capabilities, ChatRequest, ChatResponse, ChatStream, LlmError, LlmProvider, ProviderInfo,
};

/// What the notice points at: the config file the user edits, the key in
/// force (masked) and the base_url that file lists.
#[derive(Clone, Debug)]
pub struct AuthContext {
    /// Absolute path of the config file.
    pub config_path: PathBuf,
    /// The key in force, masked (`model::bootstrap::mask_key`).
    pub masked_key: String,
    /// The base_url the config file lists.
    pub config_base_url: String,
    /// Where the key came from when it is not the file (`PHIMINT_API_KEY`,
    /// `--api-key`) — the file edit would not change anything.
    pub key_source: Option<String>,
}

/// A provider failure worth a notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthFailure {
    /// The HTTP status the endpoint answered with.
    pub status: u16,
    /// The model the request went to.
    pub model: String,
    /// The endpoint the request went to.
    pub base_url: String,
}

struct WatchState {
    context: Option<AuthContext>,
    pending: Option<AuthFailure>,
    announced: Vec<AuthFailure>,
}

static WATCH: Mutex<WatchState> = Mutex::new(WatchState {
    context: None,
    pending: None,
    announced: Vec::new(),
});

/// The context for a booted config: which file holds the key, which key is
/// in force (masked) and where it came from, plus the base_url in force.
///
/// The key is masked here, at the edge: nothing downstream ever holds the
/// raw one.
pub fn boot_context(
    config_path: &Path,
    key: Option<&str>,
    key_source: Option<&str>,
    base_url: &str,
) -> AuthContext {
    AuthContext {
        config_path: config_path.to_path_buf(),
        masked_key: crate::model::bootstrap::mask_key(key.unwrap_or("")),
        config_base_url: base_url.to_string(),
        key_source: key_source.map(str::to_string),
    }
}

/// Install the watch: record the boot context and wrap the boot provider in
/// one step, so the runtime cannot end up with an unwatched provider or a
/// context that points at another file.
pub fn install(
    context: AuthContext,
    provider: Arc<dyn LlmProvider>,
    model: &str,
    base_url: &str,
) -> Arc<dyn LlmProvider> {
    set_context(context);
    watch(provider, model, base_url)
}

/// Record the boot context: which file, which masked key, which base_url.
/// Called once, after the config is loaded and the first-run gate passed.
pub fn set_context(context: AuthContext) {
    if let Ok(mut watch) = WATCH.lock() {
        watch.context = Some(context);
    }
}

/// Record a provider failure. Only 401 and 403 are worth a notice — they
/// mean "this key does not work here" — while a 500 or a network blip is the
/// endpoint's problem, not the user's config.
pub fn record_failure(err: &LlmError, model: &str, base_url: &str) {
    let Some(status) = err.status() else { return };
    if status != 401 && status != 403 {
        return;
    }
    let failure = AuthFailure {
        status,
        model: model.to_string(),
        base_url: base_url.to_string(),
    };
    if let Ok(mut watch) = WATCH.lock() {
        watch.pending = Some(failure);
    }
}

/// Take the pending notice, if there is one (the pending failure is
/// consumed either way).
///
/// Each distinct rejection is announced once per process: the config cannot
/// change under a running session, so a second copy of the same line teaches
/// the user nothing. A different endpoint (a `/model` pick) is a different
/// failure and gets its own line.
pub fn take_notice() -> Option<String> {
    let mut watch = WATCH.lock().ok()?;
    let failure = watch.pending.take()?;
    if watch.announced.iter().any(|seen| seen == &failure) {
        return None;
    }
    watch.announced.push(failure.clone());
    let context = watch.context.clone()?;
    Some(notice_text(&context, &failure))
}

/// The notice text (decision 2's runtime copy).
pub fn notice_text(context: &AuthContext, failure: &AuthFailure) -> String {
    let key_line = match &context.key_source {
        Some(source) => format!("{}  (from {})", context.masked_key, source),
        None => context.masked_key.clone(),
    };
    let mut out = format!(
        "The model endpoint rejected this request ({}). Check config.json:\n  \
         api_key:  {}\n  base_url: {}\n  file:     {}",
        failure.status,
        key_line,
        failure.base_url,
        context.config_path.display()
    );
    if failure.base_url != context.config_base_url {
        out.push_str(&format!(
            "\n  note:     config.json lists base_url {}; this request used model {}",
            context.config_base_url, failure.model
        ));
    }
    out
}

/// A provider that reports rejections to the watch on the way out.
pub struct WatchedProvider {
    inner: Arc<dyn LlmProvider>,
    model: String,
    base_url: String,
}

impl WatchedProvider {
    /// Wrap `inner` so a 401/403 on the way out reaches the notice probe.
    /// The model and endpoint travel with it: the notice has to name the
    /// endpoint that actually refused the request.
    pub fn new(inner: Arc<dyn LlmProvider>, model: &str, base_url: &str) -> Self {
        Self {
            inner,
            model: model.to_string(),
            base_url: base_url.to_string(),
        }
    }
}

#[async_trait]
impl LlmProvider for WatchedProvider {
    async fn stream(&self, request: ChatRequest) -> Result<ChatStream, LlmError> {
        match self.inner.stream(request).await {
            Ok(stream) => Ok(stream),
            Err(err) => {
                record_failure(&err, &self.model, &self.base_url);
                Err(err)
            }
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        match self.inner.chat(request).await {
            Ok(response) => Ok(response),
            Err(err) => {
                record_failure(&err, &self.model, &self.base_url);
                Err(err)
            }
        }
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn info(&self) -> ProviderInfo {
        self.inner.info()
    }
}

/// Wrap a provider for the watch. The returned provider is the one the rest
/// of the app holds — the wrapper delegates everything but the failures.
pub fn watch(inner: Arc<dyn LlmProvider>, model: &str, base_url: &str) -> Arc<dyn LlmProvider> {
    Arc::new(WatchedProvider::new(inner, model, base_url))
}

/// The lock that serializes the tests touching the global watch.
#[cfg(test)]
pub(crate) fn watch_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &LOCK
}

/// Clear the watch. The state is process-global, so a test that ran before
/// would otherwise leave its announced failures behind.
#[cfg(test)]
pub(crate) fn reset() {
    if let Ok(mut watch) = WATCH.lock() {
        watch.context = None;
        watch.pending = None;
        watch.announced.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_agent::llm_trait::{ChatMessage, FinishReason, StreamChunk};

    /// A provider that always answers with an API error.
    struct FailingProvider {
        status: u16,
    }

    #[async_trait]
    impl LlmProvider for FailingProvider {
        async fn stream(&self, _request: ChatRequest) -> Result<ChatStream, LlmError> {
            Err(LlmError::api(self.status, "invalid api key"))
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            Err(LlmError::api(self.status, "invalid api key"))
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "failing".to_string(),
                model: "m".to_string(),
                version: None,
            }
        }
    }

    /// A provider that always succeeds.
    struct OkProvider;

    #[async_trait]
    impl LlmProvider for OkProvider {
        async fn stream(&self, _request: ChatRequest) -> Result<ChatStream, LlmError> {
            let chunks: Vec<Result<StreamChunk, LlmError>> = Vec::new();
            Ok(ChatStream::new(Box::pin(futures_util::stream::iter(
                chunks,
            ))))
        }
        async fn chat(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
            Ok(ChatResponse {
                content: String::new(),
                reasoning_content: None,
                thinking_signature: None,
                tool_calls: vec![],
                usage: Default::default(),
                finish_reason: FinishReason::Stop,
                raw: None,
            })
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "ok".to_string(),
                model: "m".to_string(),
                version: None,
            }
        }
    }

    fn request() -> ChatRequest {
        ChatRequest::new(vec![ChatMessage::user("hi")])
    }

    fn context(path: &Path) -> AuthContext {
        AuthContext {
            config_path: path.to_path_buf(),
            masked_key: "sk-a…9x2".to_string(),
            config_base_url: "https://api.openai.com/v1".to_string(),
            key_source: None,
        }
    }

    /// protects: a rejected request ends in exactly one notice that names the
    /// file to open, the masked key, the endpoint that refused and the status
    /// — the user can fix the config without leaving the transcript.
    /// fails_when: the rejection is silent, or the notice points at the
    /// wrong file.
    /// no_claim: the mid-stream case (a rejection that arrives after the
    /// response started) — the provider reports those inside the stream.
    #[tokio::test]
    async fn a_rejected_request_turns_into_one_notice_naming_the_config() {
        let _guard = watch_test_lock().lock().await;
        reset();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        set_context(context(&path));

        let provider = watch(
            Arc::new(FailingProvider { status: 401 }),
            "gpt-5.4-mini",
            "https://api.openai.com/v1",
        );
        let err = match provider.stream(request()).await {
            Ok(_) => panic!("the failing provider never succeeds"),
            Err(err) => err,
        };
        assert_eq!(
            err.status(),
            Some(401),
            "the error still reaches the caller"
        );

        let notice = take_notice().expect("the rejection is announced");
        assert!(notice.contains("401"), "names the status: {notice}");
        assert!(
            notice.contains("sk-a…9x2"),
            "shows the masked key: {notice}"
        );
        assert!(
            notice.contains("https://api.openai.com/v1"),
            "names the endpoint: {notice}"
        );
        assert!(
            notice.contains(path.to_str().unwrap()),
            "names the file: {notice}"
        );
        assert_eq!(take_notice(), None, "the notice is consumed");
    }

    /// protects: the same rejection is announced once, not on every retry —
    /// the config cannot change under a running session, so a repeated line
    /// is noise. A different endpoint (a `/model` pick) is a different
    /// failure and gets its own line.
    /// fails_when: the notice repeats on every failed request, or a second
    /// endpoint's rejection is swallowed.
    #[tokio::test]
    async fn the_same_rejection_is_announced_once_and_a_new_endpoint_is_not() {
        let _guard = watch_test_lock().lock().await;
        reset();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        set_context(context(&path));

        let first = watch(
            Arc::new(FailingProvider { status: 401 }),
            "gpt-5.4-mini",
            "https://api.openai.com/v1",
        );
        let _ = first.stream(request()).await;
        assert!(take_notice().is_some(), "the first rejection is announced");
        let _ = first.stream(request()).await;
        assert_eq!(
            take_notice(),
            None,
            "the same rejection is not announced twice"
        );

        let other = watch(
            Arc::new(FailingProvider { status: 401 }),
            "deepseek-chat",
            "https://api.deepseek.com/v1",
        );
        let _ = other.stream(request()).await;
        let notice = take_notice().expect("a new endpoint is announced");
        assert!(
            notice.contains("https://api.deepseek.com/v1"),
            "names the endpoint that refused: {notice}"
        );
        assert!(
            notice.contains("config.json lists base_url"),
            "the file's base_url is named too: {notice}"
        );
    }

    /// protects: a key that does not come from the file says so — editing
    /// config.json would change nothing while `PHIMINT_API_KEY` is set.
    /// fails_when: the notice sends the user to a file that is not in charge.
    #[tokio::test]
    async fn a_key_from_the_environment_is_named_in_the_notice() {
        let _guard = watch_test_lock().lock().await;
        reset();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut ctx = context(&path);
        ctx.key_source = Some("PHIMINT_API_KEY".to_string());
        set_context(ctx);

        let provider = watch(
            Arc::new(FailingProvider { status: 403 }),
            "gpt-5.4-mini",
            "https://api.openai.com/v1",
        );
        let _ = provider.chat(request()).await;

        let notice = take_notice().expect("the rejection is announced");
        assert!(notice.contains("403"), "names the status: {notice}");
        assert!(
            notice.contains("PHIMINT_API_KEY"),
            "names where the key comes from: {notice}"
        );
    }

    /// protects: the boot context carries the masked key, the file and the
    /// base_url in force — the notice is assembled from it, so a wrong
    /// context sends the user to the wrong place. fails_when: the raw key
    /// reaches the context, or the file is not the one the loader read.
    #[test]
    fn the_boot_context_masks_the_key_and_keeps_the_file() {
        let ctx = boot_context(
            Path::new("/root/.phimint/config.json"),
            Some("sk-live-abcdef123456"),
            None,
            "https://api.openai.com/v1",
        );
        assert_eq!(
            ctx.config_path,
            PathBuf::from("/root/.phimint/config.json"),
            "the file the loader read"
        );
        assert!(ctx.masked_key.contains('…'), "masked: {}", ctx.masked_key);
        assert!(
            !ctx.masked_key.contains("abcdef123456"),
            "never the raw key: {}",
            ctx.masked_key
        );
        assert_eq!(ctx.config_base_url, "https://api.openai.com/v1");
        assert!(ctx.key_source.is_none(), "the file is in charge");
    }

    /// protects: installing the watch records the context and watches the
    /// provider in one step — the runtime cannot end up with an unwatched
    /// provider or a context that points at another file. fails_when: the
    /// provider's rejection is silent, or the notice names another file.
    #[tokio::test]
    async fn installing_the_watch_records_the_context_and_watches_the_provider() {
        let _guard = watch_test_lock().lock().await;
        reset();
        let context = boot_context(
            Path::new("/root/.phimint/config.json"),
            Some("sk-live-abcdef123456"),
            Some("PHIMINT_API_KEY"),
            "https://api.openai.com/v1",
        );
        let provider = install(
            context,
            Arc::new(FailingProvider { status: 401 }),
            "gpt-5.4-mini",
            "https://api.openai.com/v1",
        );
        let _ = provider.stream(request()).await;

        let notice = take_notice().expect("the rejection is announced");
        assert!(
            notice.contains("/root/.phimint/config.json"),
            "names the file: {notice}"
        );
        assert!(
            notice.contains("PHIMINT_API_KEY"),
            "names where the key comes from: {notice}"
        );
    }

    /// protects: only auth rejections are announced — a 500, a rate limit or
    /// a network error is not the user's config and must not send them to
    /// edit config.json. A request that succeeds says nothing at all.
    /// fails_when: a 500 produces a config notice, or a successful request
    /// produces one.
    #[tokio::test]
    async fn a_failure_that_is_not_a_rejection_stays_quiet() {
        let _guard = watch_test_lock().lock().await;
        reset();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        set_context(context(&path));

        let server_error = watch(
            Arc::new(FailingProvider { status: 500 }),
            "gpt-5.4-mini",
            "https://api.openai.com/v1",
        );
        let _ = server_error.stream(request()).await;
        assert_eq!(
            take_notice(),
            None,
            "a 500 is the endpoint's problem, not the config's"
        );

        let ok = watch(
            Arc::new(OkProvider),
            "gpt-5.4-mini",
            "https://api.openai.com/v1",
        );
        let _ = ok.stream(request()).await;
        assert_eq!(take_notice(), None, "a working request says nothing");
    }
}
