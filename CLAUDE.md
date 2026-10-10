# CLAUDE.md

## Project: phimint

A terminal AI coding agent in Rust, built on [phi-agent](https://github.com/hibuka-labs/phi-agent).
Product first; it is also the real-world stress test for the framework.

### Architecture Principle

**phimint is the product shell only.** The heavy lifting lives in the phi family
crates, each its own git repository with pure crates.io version dependencies:

- `phi-agent` — agent runtime (orchestration, sessions, streaming, tools facade)
- `phi-tui` — chat-style TUI components (transcript, markdown, composer, popups)
- `code-intel` — language intelligence (LSP, repo map, ripgrep cores)
- `phi-kernel-tools` — kernel tools (file, shell, context rotation)
- `agent-base` / `agent-works` — runtime kernel and multi-agent toolbox
- `phi-telemetry`, `log-core` — observability (session metrics, tracing sink)

phimint keeps the app shell: config, TUI wiring (app/render/handlers), update
checker, banner, and the coding-agent system prompt.

### Local development across crates

`Cargo.toml` is committed with **pure version dependencies**. To test against
local sibling checkouts, append an uncommitted override — never commit it:

```toml
# LOCAL DEV ONLY — DO NOT COMMIT
[patch.crates-io]
phi-agent = { path = "../phi-agent" }
```

Sibling repos are expected at `../phi-agent`, `../phi-tui`, `../code-intel`, etc.

### Quality gates

`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
must pass before any change is reported as done. CI runs the same gates.
