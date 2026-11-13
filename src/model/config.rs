//! Configuration for multi-model support.
//!
//! `ModelConfig` holds the configuration for model tiers and scene routing.
//! It supports loading from JSON config file, environment variables, and CLI arguments.
//!
//! # Configuration Format
//!
//! The JSON config supports flexible tier definitions:
//!
//! ## Simple (same provider):
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-xxx",
//!   "main": "gpt-5.4-mini",
//!   "lite": "gpt-4o-mini",
//!   "advanced": "o1-preview"
//! }
//! ```
//!
//! ## Full (different providers):
//! ```json
//! {
//!   "main": { "model": "gpt-5.4-mini", "base_url": "https://api.openai.com/v1", "api_key": "sk-openai" },
//!   "lite": { "model": "deepseek-chat", "base_url": "https://api.deepseek.com/v1", "api_key": "sk-deepseek" },
//!   "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant" }
//! }
//! ```
//!
//! ## Mixed:
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-xxx",
//!   "main": "gpt-5.4-mini",
//!   "lite": "gpt-4o-mini",
//!   "advanced": { "model": "claude-opus-4", "base_url": "https://api.anthropic.com", "api_key": "sk-ant" }
//! }
//! ```
//!
//! ## Selection pointer (`model`):
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-xxx",
//!   "main": "gpt-5.4-mini",
//!   "model": "o1-preview"
//! }
//! ```
//! A `/model` pick writes only the top-level `model` key (same `TierConfig`
//! shape: bare name or full object). It selects the effective `main` without
//! touching the `main`/`lite`/`advanced` definitions; deleting it returns to
//! the configured `main`. Absent = follow the configuration (zero migration).

use std::collections::HashMap;
use std::path::PathBuf;

use phi_tui::popup_list::{PopupStyle, WidthSpec};
use serde::Deserialize;

/// The resolved endpoint tuple for one model: `(model, base_url, api_key,
/// protocol)`. One spelling across the family — the resolver, the store's
/// tier table and persistence all talk in this shape.
pub type EndpointTuple = (String, String, String, String);

/// Tier configuration - can be a simple model name or full config.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum TierConfig {
    /// Simple: just model name, inherits base_url/api_key/protocol from parent
    Simple(String),
    /// Full: has own model, base_url, api_key, protocol
    Full {
        model: String,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        protocol: Option<String>,
    },
}

impl TierConfig {
    /// Get the model name.
    pub fn model(&self) -> &str {
        match self {
            TierConfig::Simple(model) => model,
            TierConfig::Full { model, .. } => model,
        }
    }

    /// Resolve to (model, base_url, api_key, protocol) using defaults.
    pub fn resolve(
        &self,
        default_url: &str,
        default_key: &str,
        default_protocol: &str,
    ) -> EndpointTuple {
        match self {
            TierConfig::Simple(model) => (
                model.clone(),
                default_url.to_string(),
                default_key.to_string(),
                default_protocol.to_string(),
            ),
            TierConfig::Full {
                model,
                base_url,
                api_key,
                protocol,
            } => {
                let url = base_url.clone().unwrap_or_else(|| default_url.to_string());
                let key = api_key.clone().unwrap_or_else(|| default_key.to_string());
                let proto = protocol
                    .clone()
                    .unwrap_or_else(|| default_protocol.to_string());
                (model.clone(), url, key, proto)
            }
        }
    }
}

/// Configuration for model tiers and routing.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    /// Default base_url (tier can override)
    #[serde(default)]
    pub base_url: Option<String>,

    /// Default api_key (tier can override)
    #[serde(default)]
    pub api_key: Option<String>,

    /// Default protocol (tier can override)
    /// Values: openai, anthropic, deepseek, aliyun, moonshot, gemini, ollama
    #[serde(default)]
    pub protocol: Option<String>,

    /// Main tier (required)
    pub main: TierConfig,

    /// Lite tier (optional, falls back to main)
    #[serde(default)]
    pub lite: Option<TierConfig>,

    /// Advanced tier (optional, falls back to main)
    #[serde(default)]
    pub advanced: Option<TierConfig>,

    /// Selection pointer (`model` key): the switched-to model. `None` = no
    /// switch has been made and the configured `main` definition is the
    /// effective main. The three fields above are DEFINITIONS — a switch
    /// writes only this pointer, never a definition. Parse is lenient
    /// (`lenient_pointer`): a malformed value warns and reads as no pointer
    /// instead of failing the whole file.
    #[serde(default, deserialize_with = "lenient_pointer")]
    pub model: Option<TierConfig>,

    /// Scene-to-tier mapping overrides (optional)
    #[serde(default)]
    pub scene_tiers: Option<HashMap<String, String>>,

    /// Update checker configuration (optional, defaults apply if absent).
    #[serde(default)]
    pub update: UpdateConfig,

    /// UI configuration (optional, defaults apply if absent).
    #[serde(default)]
    pub ui: UiConfig,

    /// CodeAct engine wiring (optional, defaults apply if absent — `mode`
    /// defaults to `tool`, today's shipped behavior).
    #[serde(default)]
    pub code_exec: CodeExecConfig,
}

/// `code_exec` — the runtime switch for the code road (M3 T10d, disable
/// layer ③ on top of the `code-exec` compile feature).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CodeExecConfig {
    /// Which road code work takes. Absent → [`CodeExecMode::Tool`].
    #[serde(default)]
    pub mode: CodeExecMode,
}

/// `code_exec.mode` — which road code work takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodeExecMode {
    /// The `code_cell` tool on the tool-call road (shipped M2 behavior).
    #[default]
    Tool,
    /// The CodeAct action backend on text turns (M3 road): a code step is
    /// the model's action itself, not a tool call.
    Act,
    /// No code road: `code_cell` is not registered and text turns never
    /// reach a code backend.
    Off,
}

impl CodeExecMode {
    /// The road this binary can actually deliver (disable layer ②): without
    /// the `code-exec` compile feature there is no engine at all, so every
    /// mode resolves to [`CodeExecMode::Off`] — prompt and assembly agree.
    #[cfg(not(feature = "code-exec"))]
    pub fn resolved(self) -> Self {
        let _ = self;
        CodeExecMode::Off
    }

    /// With the engine compiled in, the config says what it says.
    #[cfg(feature = "code-exec")]
    pub fn resolved(self) -> Self {
        self
    }
}

/// UI configuration: the completion popup band, the tips line, and the exit
/// transcript replay.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct UiConfig {
    #[serde(default)]
    pub popup: PopupConfig,
    #[serde(default)]
    pub tips: TipsConfig,
    /// What the restored terminal keeps when phimint exits.
    #[serde(default)]
    pub exit_dump: ExitDumpMode,
}

/// `ui.exit_dump` — what the restored terminal shows after phimint quits.
///
/// phimint draws on the alternate screen, whose content the terminal discards
/// on exit, so "keep the record" is a replay printed to the primary screen
/// (see `phi_agent_tui::exit_dump`). The mode is the replay's completeness —
/// its vocabulary is the shell's; the config parsing that feeds it is product.
pub use phi_agent_tui::ExitDumpMode;

/// `ui.popup` — the `@`/`/` popup band. Missing fields take the widget defaults.
#[derive(Debug, Clone, Deserialize)]
pub struct PopupConfig {
    /// Draw a rounded border around the band.
    #[serde(default)]
    pub frame: bool,
    /// `"fill"` (match the composer width) or a column count.
    #[serde(default)]
    pub width: WidthConfig,
    /// Maximum rows shown.
    #[serde(default = "default_popup_height")]
    pub height: u16,
    /// Draw the dim rule under the rows.
    #[serde(default = "default_true")]
    pub separator: bool,
}

fn default_popup_height() -> u16 {
    PopupStyle::default().height
}

fn default_true() -> bool {
    true
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            frame: false,
            width: WidthConfig::default(),
            height: default_popup_height(),
            separator: true,
        }
    }
}

/// `"fill"` or a column count.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WidthConfig {
    Fill(String),
    Fixed(u16),
}

impl Default for WidthConfig {
    fn default() -> Self {
        Self::Fill("fill".to_string())
    }
}

impl PopupConfig {
    /// Convert to the widget style, validating `width` and `height`.
    pub fn to_style(&self) -> Result<PopupStyle, ConfigError> {
        let width = match &self.width {
            WidthConfig::Fixed(n) => WidthSpec::Fixed(*n),
            WidthConfig::Fill(s) if s == "fill" => WidthSpec::Fill,
            WidthConfig::Fill(other) => {
                return Err(ConfigError::ParseError(format!(
                    "ui.popup.width must be \"fill\" or a column count, got {other:?}"
                )));
            }
        };
        // Reject rather than clamp, matching `width` above: `height: 0` means
        // "show no rows", which leaves the band as bare chrome (an empty border
        // box when `frame` is on). Silently reinterpreting it hides the mistake;
        // the widget floors its content at one row anyway, so this only makes
        // the config surface honest.
        if self.height == 0 {
            return Err(ConfigError::ParseError(
                "ui.popup.height must be at least 1, got 0".to_string(),
            ));
        }
        Ok(PopupStyle {
            frame: self.frame,
            width,
            height: self.height,
            separator: self.separator,
            ..PopupStyle::default()
        })
    }
}

/// `ui.tips` — the rotating tips line under the process status line.
#[derive(Debug, Clone, Deserialize)]
pub struct TipsConfig {
    /// Show the tips line at all.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tips manifest endpoint URLs (tried in order, 3s timeout each).
    /// Each is a full URL to a tips JSON file.
    #[serde(default = "default_tips_endpoints")]
    pub endpoints: Vec<String>,
    /// Minimum seconds between cloud refresh attempts.
    #[serde(default = "default_tips_refresh_interval")]
    pub refresh_interval_secs: u64,
}

impl Default for TipsConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            endpoints: default_tips_endpoints(),
            refresh_interval_secs: default_tips_refresh_interval(),
        }
    }
}

fn default_tips_endpoints() -> Vec<String> {
    vec![
        // Primary: GitHub (global). Fallback: Gitee (mainland China).
        // The checker tries endpoints in order, 3s timeout each.
        "https://github.com/hibuka-labs/phimint/releases/latest/download/tips.json".to_string(),
        "https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/tips.json".to_string(),
    ]
}

fn default_tips_refresh_interval() -> u64 {
    86_400
}

/// Configuration for the update checker.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateConfig {
    /// Manifest endpoint URLs (tried in order, 3s timeout each).
    /// Each is a full URL to a manifest JSON file.
    #[serde(default = "default_endpoints")]
    pub endpoints: Vec<String>,

    /// Release channel: "stable" or "beta".
    #[serde(default = "default_channel")]
    pub channel: String,

    /// Whether to check for updates on startup.
    #[serde(default = "default_auto_check")]
    pub auto_check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            endpoints: default_endpoints(),
            channel: default_channel(),
            auto_check: default_auto_check(),
        }
    }
}

fn default_endpoints() -> Vec<String> {
    vec![
        // Primary: GitHub (global). Fallback: Gitee (mainland China).
        // The checker tries endpoints in order, 3s timeout each.
        "https://github.com/hibuka-labs/phimint/releases/latest/download/manifest.json".to_string(),
        "https://gitee.com/chenkangzeng_admin/phimint/raw/release-metadata/manifest.json"
            .to_string(),
    ]
}

fn default_channel() -> String {
    "stable".to_string()
}

fn default_auto_check() -> bool {
    true
}

/// Lenient parse of the `model` selection pointer. The key is user-editable
/// and older files may carry anything there (before this key was activated it
/// was ignored), so a value that is not a [`TierConfig`] shape must never
/// brick startup: warn and read it as "no pointer". `null` reads as no
/// pointer without a warning.
fn lenient_pointer<'de, D>(deserializer: D) -> Result<Option<TierConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    match serde_json::from_value::<TierConfig>(value) {
        Ok(pointer) => Ok(Some(pointer)),
        Err(err) => {
            tracing::warn!(
                "config `model` pointer is malformed ({err}); running without a pointer"
            );
            Ok(None)
        }
    }
}

impl ModelConfig {
    /// Load configuration from a JSON file (supports JSON5 with comments).
    pub fn from_file(path: &PathBuf) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::IoError(format!("Failed to read config file: {}", e)))?;
        let config: ModelConfig = json5::from_str(&content)
            .map_err(|e| ConfigError::ParseError(format!("Failed to parse config: {}", e)))?;
        Ok(config)
    }

    /// The config file path, resolved the same way loading resolves it:
    /// `LLM_CONFIG` when it points at a real file, else the first existing
    /// default location. Shared by loading and `/model` persistence so a
    /// pick is always saved to the file the next boot will read.
    pub fn resolve_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("LLM_CONFIG") {
            let path = PathBuf::from(path);
            if path.exists() {
                return Some(path);
            }
        }
        let home = dirs::home_dir()?;
        [
            home.join(".phimint").join("config.json"),
            home.join(".config").join("phimint").join("config.json"),
        ]
        .into_iter()
        .find(|p| p.exists())
    }

    /// Load configuration from default locations.
    ///
    /// Checks in order (see [`Self::resolve_path`]):
    /// 1. LLM_CONFIG environment variable
    /// 2. ~/.phimint/config.json
    /// 3. ~/.config/phimint/config.json
    pub fn from_default_location() -> Result<Option<Self>, ConfigError> {
        match Self::resolve_path() {
            Some(path) => Ok(Some(Self::from_file(&path)?)),
            None => Ok(None),
        }
    }

    /// Get the resolved endpoint tuple for a tier — THE one resolution rule
    /// (there must never be a second: boot provider, store tier table and
    /// [`Self::validate`] all resolve through here). The effective `main` is
    /// the `model` pointer when set (a selection), else the configured
    /// `main`; `lite`/`advanced` use their own definition and fall back to
    /// the effective `main` when unconfigured (an unconfigured tier follows a
    /// switch).
    ///
    /// Endpoint fields inherit along one chain — the tier's own value when
    /// it has one, then the top-level default, then the **configured**
    /// `main`'s field (never the effective one: for the pointer arm that is
    /// the very tier being resolved, so anchoring there is circular). A
    /// name-only selection (`--model`, a `model` pointer) therefore keeps the
    /// configured main's endpoint instead of losing it — and a boot-refusing
    /// "base_url must be set at top level or in main tier" over a config that
    /// sets it in `main` can no longer happen (review findings
    /// p2/outside-voice#2 and p2/red-team#1).
    ///
    /// A field that IS set wins even when it is blank: `api_key: ""` is a
    /// deliberate blank pin (it is what the provider would be built from), so
    /// the chain must not rescue it from the level above — the same rule
    /// [`TierConfig::resolve`] applies.
    pub fn tier_config(&self, tier: &str) -> Option<EndpointTuple> {
        let effective_main = self.model.as_ref().unwrap_or(&self.main);
        let definition = match tier {
            "main" => Some(effective_main),
            "lite" | "advanced" => Some(self.tier_def(tier).unwrap_or(effective_main)),
            _ => None,
        }?;
        Some(self.resolve_with_anchor(definition))
    }

    /// Resolve one tier definition against the endpoint chain: the
    /// definition's own field when it has one → the top-level default → the
    /// configured `main`'s field. See [`Self::tier_config`].
    ///
    /// The anchor's pinned `api_key`/`protocol` belong to the anchor's
    /// *endpoint*: a tier that resolves onto a different `base_url` must never
    /// inherit them, or the main tier's credential travels to a third-party
    /// host and the wrong wire format speaks to it (review finding
    /// v11/adversarial-native#4). Key and protocol therefore ride the chain
    /// only when the resolved URL is the anchor's; otherwise they come from
    /// the tier or the top level alone.
    fn resolve_with_anchor(&self, tc: &TierConfig) -> EndpointTuple {
        // The anchor: the configured `main` resolved own → top-level.
        // `Self::resolve`'s two-layer form is exactly that.
        let anchor = self.main.resolve(
            self.base_url.as_deref().unwrap_or(""),
            self.api_key.as_deref().unwrap_or(""),
            self.protocol.as_deref().unwrap_or(""),
        );
        // Presence, not emptiness: a set-but-blank field is deliberate (a
        // blank pinned key is what the provider would be built from), so it
        // wins over the level above — the same rule `TierConfig::resolve`
        // applies to a tier's own fields.
        let chain = |own: &Option<String>, top: Option<&str>, anchored: &str| -> String {
            own.clone()
                .or_else(|| top.map(|s| s.to_string()))
                .unwrap_or_else(|| anchored.to_string())
        };
        let no_anchor = |own: &Option<String>, top: Option<&str>| -> String {
            own.clone()
                .or_else(|| top.map(|s| s.to_string()))
                .unwrap_or_default()
        };
        let top_url = self.base_url.as_deref();
        let top_key = self.api_key.as_deref();
        let top_proto = self.protocol.as_deref();
        let (model, url, own_key, own_proto) = match tc {
            TierConfig::Simple(model) => {
                (model.clone(), chain(&None, top_url, &anchor.1), None, None)
            }
            TierConfig::Full {
                model,
                base_url,
                api_key,
                protocol,
            } => (
                model.clone(),
                chain(base_url, top_url, &anchor.1),
                api_key.clone(),
                protocol.clone(),
            ),
        };
        let on_anchor = url == anchor.1;
        let key = if on_anchor {
            chain(&own_key, top_key, &anchor.2)
        } else {
            no_anchor(&own_key, top_key)
        };
        let proto = if on_anchor {
            chain(&own_proto, top_proto, &anchor.3)
        } else {
            no_anchor(&own_proto, top_proto)
        };
        (model, url, key, proto)
    }

    /// The tier's declaration as configured, no fallback: what the file SAYS
    /// (`main` always has one; `lite`/`advanced` only when configured). The
    /// picker's shelf and tier tags read declarations — a selection never
    /// moves a definition.
    pub fn tier_def(&self, tier: &str) -> Option<&TierConfig> {
        match tier {
            "main" => Some(&self.main),
            "lite" => self.lite.as_ref(),
            "advanced" => self.advanced.as_ref(),
            _ => None,
        }
    }

    /// The configured `main` definition resolved with the top-level defaults
    /// — the tuple「clear selection」rolls back to. The `model` pointer and
    /// `--model` are selections; this is the definition underneath them.
    pub fn configured_main(&self) -> EndpointTuple {
        self.main.resolve(
            self.base_url.as_deref().unwrap_or(""),
            self.api_key.as_deref().unwrap_or(""),
            self.protocol.as_deref().unwrap_or(""),
        )
    }

    /// Apply the CLI `--model` session override: it writes the `model`
    /// selection slot — the same slot a file pointer loads into, so the
    /// precedence CLI > pointer > configured `main` falls out of one rule.
    /// Session-scoped and never persisted: the file keeps its own pointer for
    /// the next boot. The configured `main` definition is NOT rewritten (the
    /// old behavior lost it for the session and dropped a picker row).
    pub fn apply_cli_model(&mut self, model: Option<&str>) {
        if let Some(model) = model {
            self.model = Some(TierConfig::Simple(model.to_string()));
        }
    }

    /// Get all configured tier names.
    pub fn tier_names(&self) -> Vec<&str> {
        let mut names = vec!["main"];
        if self.lite.is_some() {
            names.push("lite");
        }
        if self.advanced.is_some() {
            names.push("advanced");
        }
        names
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Validate main tier has a model
        if self.main.model().is_empty() {
            return Err(ConfigError::ValidationError(
                "main tier model cannot be empty".to_string(),
            ));
        }

        // Validate base_url is set (either at top level or in main tier)
        let (_, url, _, _) = self.tier_config("main").unwrap();
        if url.is_empty() {
            return Err(ConfigError::ValidationError(
                "base_url must be set at top level or in main tier".to_string(),
            ));
        }

        Ok(())
    }
}

/// CLI arguments for model configuration.
#[derive(Debug, Clone, Default)]
pub struct CliModelArgs {
    /// Main model
    pub model: Option<String>,
    /// Lite model
    pub lite_model: Option<String>,
    /// Advanced model
    pub advanced_model: Option<String>,
    /// API base URL
    pub base_url: Option<String>,
    /// API key
    pub api_key: Option<String>,
}

/// Configuration error types.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    IoError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

#[cfg(test)]
mod tests;
