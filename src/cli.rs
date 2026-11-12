//! The phimint CLI surface: every flag and subcommand, extracted from
//! main.rs so the parse surface is testable and the binary stays a thin
//! shell.
//!
//! `init` and `config` (show/set) are the install-onboarding commands
//! (notes/install-onboarding-design.md, decisions 2/3/6): the installer
//! calls `phimint init --if-missing`, and a user who prefers the command
//! line to an editor fills the key with `phimint config set api_key <key>`.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::model::config_cmd::WRITABLE_MEMBERS;
use crate::session::scope;

#[derive(Parser)]
#[command(
    name = "phimint",
    version,
    about = "AI coding agent built on phi-agent"
)]
pub struct Cli {
    /// Workspace directory to operate in (default: current directory)
    #[arg(short, long, default_value = ".")]
    pub workspace: PathBuf,

    /// Model name (session-scoped selection: overrides the `model` pointer
    /// and the configured main tier for this run; never persisted)
    #[arg(long)]
    pub model: Option<String>,

    /// Lite model name (overrides lite tier)
    #[arg(long)]
    pub lite_model: Option<String>,

    /// Advanced model name (overrides advanced tier)
    #[arg(long)]
    pub advanced_model: Option<String>,

    /// API base URL (overrides default base_url)
    #[arg(long)]
    pub base_url: Option<String>,

    /// API key (overrides default api_key)
    #[arg(long)]
    pub api_key: Option<String>,

    /// API protocol (overrides default protocol)
    #[arg(long)]
    pub protocol: Option<String>,

    /// Model config file path (overrides default locations)
    #[arg(long)]
    pub config: Option<String>,

    /// Approval mode (initial): `auto` (default), `ask` (prompt on writes/risky
    /// shell), or `deny` (reject all writes). Shift+Tab toggles auto/ask at runtime.
    #[arg(long, default_value = "auto")]
    pub approval: String,

    /// Shell command timeout in milliseconds
    #[arg(long, default_value_t = 120_000)]
    pub shell_timeout_ms: u64,

    /// Session ID (defaults to PHI_SESSION_ID env, else auto-generated).
    /// Reuse the same ID across runs to append turns to the same log directory.
    #[arg(long, conflicts_with = "resume")]
    pub session: Option<String>,

    /// Resume a previous session. Shows an interactive picker to choose from
    /// recent sessions. Conflicts with --session.
    #[arg(long)]
    pub resume: bool,

    /// Log level for the session.log file (debug/info/warn/error)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Color scheme for the startup banner: `auto` (default, detects terminal
    /// background), `dark` or `light` (override).
    #[arg(long, default_value = "auto")]
    pub color_scheme: String,

    /// Show the startup banner (default: `on`; set to `off` to suppress it).
    #[arg(long, default_value = "on")]
    pub banner: String,

    /// Thinking/reasoning budget in tokens (default: 8192).
    /// Controls how many tokens the model can use for internal reasoning.
    #[arg(long, default_value_t = 8192)]
    pub thinking_budget: u64,

    /// Reasoning effort level: none/low/medium/high/xhigh (default: medium).
    /// Controls the depth of model's reasoning. Higher = more thorough but slower.
    #[arg(long, default_value = "medium")]
    pub reasoning_effort: String,

    /// Work-room token budget for context windows (tokens of conversation
    /// space ABOVE the fixed window base). Window rotation + history/notes
    /// tools replace LLM summarization; this is now the default mode. Bare
    /// `--token-budget` or omitting the flag uses the default (210K,
    /// aligned with Codex's ~256K context window × 90% minus base overhead).
    #[arg(
        long,
        num_args(0..=1),
        default_missing_value = "210000",
        default_value = "210000"
    )]
    pub token_budget: usize,

    /// Days to retain session history and notes (default: 30).
    /// Expired data is cleaned up at startup.
    ///
    /// Deliberately tied to `scope::RECENT_DAYS`: the resume
    /// picker's default view reaches back a month, so the data has to
    /// survive a month or the window has nothing to show.
    #[arg(long, default_value_t = scope::RECENT_DAYS as i64)]
    pub session_retention_days: i64,

    /// Skip the automatic update check on startup.
    #[arg(long)]
    pub no_update_check: bool,

    /// Subcommands (`phimint update`) that run without starting the TUI.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Check for a new phimint version and update this binary.
    ///
    /// Standalone installs (curl installer) self-replace; installs owned by
    /// brew/npm/cargo print their package manager's upgrade command instead.
    Update {
        /// Only report whether an update is available; never download.
        #[arg(long)]
        check: bool,
    },

    /// Remove phimint and the data it left behind.
    ///
    /// The binary goes back to the channel that owns it (brew/npm/cargo run
    /// their own uninstall; a standalone install is deleted here). Then
    /// `~/.phimint/` is removed — that tree holds your API key and sessions,
    /// which no package manager knows about — and the Windows user-PATH entry
    /// the installer added is dropped.
    Uninstall {
        /// Keep ~/.phimint (config, sessions, history, notes).
        #[arg(long)]
        keep_data: bool,

        /// Skip the confirmation prompt.
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Write the config skeleton without starting the TUI.
    ///
    /// The installer runs `phimint init --if-missing` after placing the
    /// binary; an existing config is never touched.
    Init {
        /// Only report when the file is created — quiet when a config is
        /// already there (the installer's mode).
        #[arg(long)]
        if_missing: bool,
    },

    /// Inspect or edit the config file without starting the TUI.
    Config {
        /// `show` (default) prints the config with the API key masked;
        /// `set` writes one member.
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print the resolved configuration with the API key masked.
    Show,

    /// Set one top-level config member (api_key, base_url, protocol, main).
    Set {
        /// The member to write — the only ones `set` will touch.
        #[arg(value_parser = clap::builder::PossibleValuesParser::new(WRITABLE_MEMBERS))]
        key: String,

        /// The new value. For `api_key`, pass `-` to read the secret from
        /// stdin instead — an argv secret lands in shell history and process
        /// listings.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// protects: the onboarding subcommands parse exactly as documented —
    /// `init` (with `--if-missing`), `config show`, and `config set` whose
    /// key is restricted to the four writable members. fails_when:
    /// `config set model` (the /model pointer's slot) or any unknown key
    /// parses, or `config set` swallows a missing value. no_claim: what the
    /// commands do once parsed (model::config_cmd pins that).
    #[test]
    fn the_init_and_config_subcommands_parse_and_the_set_key_is_restricted() {
        let cli = Cli::try_parse_from(["phimint", "init"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Init { if_missing: false })
        ));
        let cli = Cli::try_parse_from(["phimint", "init", "--if-missing"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Init { if_missing: true })
        ));

        let cli = Cli::try_parse_from(["phimint", "config", "show"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Config {
                action: Some(ConfigAction::Show)
            })
        ));

        // Bare `phimint config` is the show command.
        let cli = Cli::try_parse_from(["phimint", "config"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Config { action: None })
        ));

        let cli = Cli::try_parse_from(["phimint", "config", "set", "api_key", "sk-test"]).unwrap();
        match cli.command {
            Some(Commands::Config {
                action: Some(ConfigAction::Set { key, value }),
            }) => {
                assert_eq!(key, "api_key");
                assert_eq!(value, "sk-test");
            }
            _ => panic!("expected `config set api_key sk-test`"),
        }

        // The key whitelist is enforced at parse time.
        assert!(
            Cli::try_parse_from(["phimint", "config", "set", "model", "m"]).is_err(),
            "the /model pointer's slot is not a config-set key"
        );
        assert!(
            Cli::try_parse_from(["phimint", "config", "set", "bogus", "v"]).is_err(),
            "an unknown key never parses"
        );
        assert!(
            Cli::try_parse_from(["phimint", "config", "set", "api_key"]).is_err(),
            "a value is required"
        );
        assert!(
            Cli::try_parse_from(["phimint", "config", "bogus"]).is_err(),
            "an unknown config subcommand never parses"
        );
    }

    /// protects: moving the CLI out of main.rs changed nothing — every
    /// legacy flag, default and conflict parses exactly as before.
    /// fails_when: the extraction silently renames a flag, drops a default
    /// or loses the `--session`/`--resume` conflict. no_claim: runtime
    /// behavior behind the flags (main.rs wires those).
    #[test]
    fn the_extraction_kept_the_legacy_surface_intact() {
        let cli = Cli::try_parse_from(["phimint"]).unwrap();
        assert_eq!(cli.workspace, std::path::PathBuf::from("."));
        assert_eq!(cli.token_budget, 210_000);
        assert_eq!(cli.approval, "auto");
        assert!(!cli.no_update_check);
        assert!(cli.command.is_none());

        let cli = Cli::try_parse_from([
            "phimint",
            "--workspace",
            "/w",
            "--model",
            "m",
            "--api-key",
            "k",
            "--config",
            "/c.json",
            "--approval",
            "deny",
            "--token-budget",
            "7000",
            "--no-update-check",
        ])
        .unwrap();
        assert_eq!(cli.workspace, std::path::PathBuf::from("/w"));
        assert_eq!(cli.model.as_deref(), Some("m"));
        assert_eq!(cli.api_key.as_deref(), Some("k"));
        assert_eq!(cli.config.as_deref(), Some("/c.json"));
        assert_eq!(cli.approval, "deny");
        assert_eq!(cli.token_budget, 7000);
        assert!(cli.no_update_check);

        let cli = Cli::try_parse_from(["phimint", "update", "--check"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Update { check: true })
        ));

        let cli = Cli::try_parse_from(["phimint", "uninstall", "--keep-data", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Uninstall {
                keep_data: true,
                yes: true
            })
        ));

        assert!(
            Cli::try_parse_from(["phimint", "--session", "s", "--resume"]).is_err(),
            "the session/resume conflict survived the move"
        );
    }
}
