# Changelog

All notable changes to phimint.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
