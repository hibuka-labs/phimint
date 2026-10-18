# Changelog

All notable changes to phimint.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0] — 2026-10-18

### Added

- **External tools** — plug your own tools into the agent. Three shapes behind
  one phi-tool contract (WIT + Component Model): MCP servers, user scripts, and
  wasm plugin packs. `/connect` attaches an MCP server (Streamable HTTP and
  stdio), `/tools` shows every tool's source, namespace and tier with one-key
  disable, and `plugin add` / `plugin remove` manage plugin packs with the
  capability manifest shown at confirm time. A built-in catalog offers common
  servers as one-line adds; a setup wizard parses pasted config and extracts
  secrets into `${ENV:VAR}` references — literal secrets are never written to
  disk, and a missing variable is named on the spot at connect time.
- **`add_tool`** — the agent can write and register a tool for you (root-only).
  The confirmation card shows the full picture (argv, absolute path, timeout)
  before anything lands; literal secrets and malformed entries never reach the
  card. Registered tools hot-refresh into the next turn's tool list.
- **External-tool approval ledger** — per-tool tiers (always-ask / allow / deny)
  keyed by identity (`mcp.` / `script.` / `plugin.` namespaces), persisted to
  `~/.phimint/approvals.json`, fail-closed. Sub-agents never see external tools
  (the namespaces are dropped from spawn sets with a warning).
- **Workspace-scoped resume** — `/resume` restores a session with its full
  history and its home workspace (cwd + provenance) back in place.
- On quit the session is replayed onto the terminal, so the scrollback shows
  what just happened.
- A tips line under the transcript and a flat wordmark lockup in the banner.

### Changed

- The bottom status bar is Claude Code style: the mode badge reads `auto mode
  on` / `ask mode on` / `deny mode on` with `(shift+tab to cycle)` behind it
  (auto and ask share the brand warm color; only deny is red), and the right
  slot carries the upgrade hint or the scroll position. The composer sits
  flush between its rules, with no blank rows around it.

### Fixed

- Popup decision buttons are never clipped: the popup's bottom row is
  guaranteed visible and overflow trims away from it.
- Approval popups can no longer be answered by a single stray key — decision
  keys need focus or a modifier.
- Long notices no longer garble the transcript when the terminal is resized:
  old wrap continuation fragments are dropped precisely (wide chars included).
- `Ctrl+L` forces a repaint, and resize/paste no longer lose visibility —
  the user-turn state is committed at the send site, so the spinner can't
  outlive a turn (phantom spinner fix).
- Script tools whose file is missing (or not executable) are refused at
  install time instead of installing silently.
- `add_tool` no longer trips a fake 600s timeout when the tool registers
  itself mid-call (registry lock fix upstream), and the TUI no longer hard
  freezes when an MCP child clobbers the terminal (children spawn detached;
  a termios guard probes for external clobber).

## [0.4.1] — 2026-10-12

### Fixed

- npm installs on Windows actually get a binary. The Windows platform package
  is now `phimint-windows-x64`; the old name `phimint-win32-x64` was blocked
  by npm's spam detection on every attempt since 0.2.0 and never published,
  so `optionalDependencies` silently skipped it and `npm install -g phimint`
  left no binary behind. The launcher maps npm's `win32` platform token to
  the new package name.

## [0.4.0] — 2026-10-11

### Added

- `Shift+Tab` toggles the approval mode mid-session (`auto` ⇄ `ask`; from
  `deny` the first press enters `ask`). The flip takes effect on the next tool
  call — no restart, no rewiring. Entering `auto` resolves any pending prompts
  so a run is never left blocked on a confirmation. The status bar badges the
  live mode: `[auto]` green / `[ask]` yellow / `[deny]` red.
- Write-capable sub-agents. `spawn_agent` can request write capability per
  spawn (`tools: "write"`, or a `coder`/`tester` preset); default spawns stay
  read-only. Write-capable children work disjoint file sets under a file lock
  (a colliding file reports `file locked by <agent>`), and in `ask` every
  child write prompts in a popup naming the requesting sub-agent.

### Changed

- Approval wiring is now a single live gate (policy + handler behind one
  shared mode switch) instead of mode-fixed wiring built at startup.
  `--approval` only sets the initial mode; sub-agents follow the live mode
  through the parent-policy delegation chain.

## [0.3.0] — 2026-10-10

### Added

- `phimint uninstall` — remove phimint and the data it left behind. The binary
  goes back to whichever channel installed it (`brew uninstall` / `npm
  uninstall` / `cargo uninstall`, or self-delete for standalone installs), then
  `~/.phimint/` (config including the API key, sessions, history, notes) is
  deleted. `--keep-data` leaves the data directory alone; `--yes` skips the
  confirmation prompt. On Windows it also drops the PATH entry `install.ps1`
  added.

### Removed

- The parked verify-before-deliver gate (`src/gate.rs`) and its `verify` tool —
  the middleware never ran in the agent loop; compile/test feedback belongs to
  the agent's normal build loop.

### Fixed

- Install one-liners in the READMEs: raw.githubusercontent serves 451 for
  `.sh` / `.ps1`, so the mainland-China lines now fetch from
  `releases/download/latest`; Homebrew installs via the tap as
  `brew install hibuka-labs/phimint/phimint`.

## [0.2.0] — 2026-10-07

Distribution release — install phimint without a Rust toolchain, on every
major platform, with mainland-China mirrors.

### Added

- Prebuilt binaries on every release: macOS (arm64 / x64), Linux (x64 /
  arm64) and Windows (x64), published via GitHub Releases
- One-click installers for machines without Rust: `install.sh` (Unix) and
  `install.ps1` (Windows) — download, checksum-verify and install to
  `~/.local/bin`, no build step
- Dual-source distribution: GitHub + Gitee mirror for mainland China.
  Installers, release assets and the update manifest all fall back to Gitee;
  `PHIMINT_MIRROR=gitee|github` forces a side
- Homebrew support: `brew install phimint` from the `hibuka-labs/phimint` tap
- npm support: `npm install -g phimint` (pnpm / yarn work too) — esbuild-style
  platform packages with no postinstall scripts, so registry mirrors pick
  them up automatically
- `phimint update` — upgrade from inside the app: self-replaces standalone
  installs (sha256-verified download + atomic replace), or prints the right
  upgrade command for brew / npm / cargo installs. `phimint update --check`
  reports without upgrading
- Release manifest v1 extended with per-platform `sha256` (older clients
  ignore the field); update checker falls back across manifest endpoints
- Windows support as a first-class target (installer, binaries, self-update
  guidance)

### Changed

- `install.sh` now installs prebuilt binaries instead of building from source

## [0.1.0] — 2026-10-07

First public release — a terminal AI coding agent built on phi-agent.

### Added

- Streaming TUI (ratatui + crossterm): fixed bottom composer, token-by-token
  streaming, inline tool calls, live shell output, approval popups, startup
  banner with light/dark terminal detection
- Slash commands: `.claude/skills`-compatible skill injection; `/resume` with an
  interactive session picker
- Pull-based context: `repo_map` (tree-sitter index) + `search_content` (ripgrep)
- LSP fast inner loop: lazily started per-language servers with
  `file:line:col` diagnostics and shell fallback
- Multi-agent fan-out with push-based fan-in: `spawn_agent` sub-agents deliver
  full reports at end of turn; live task panel with a hard per-task timeout
- Three approval modes (`auto` / `ask` / `deny`); sub-agents inherit the mode
  and can never write
- Guardrails: reasoning-only spin correction, max-turns nudge, provider
  truncation guard, tool-output size cap
- Multi-language support via `code-intel`: Rust, TypeScript, JavaScript, C, C++
- Session observability: per-turn event JSONL, `session.log`, token metrics
- Auto-update checker with `/upgrade` command and manifest endpoint fallback
- Dark/light theme support; transcript typography with wide-glyph (CJK) wrapping
