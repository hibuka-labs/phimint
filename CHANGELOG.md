# Changelog

All notable changes to phimint.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
