# Contributing to phimint

Thanks for your interest in contributing! This guide gets you from clone to PR in minutes.

## Quick Start (5 min)

```bash
git clone https://github.com/hibuka-labs/phimint.git
cd phimint
cargo build        # pulls dependencies from crates.io automatically
cargo test         # make sure everything passes
```

**That's it.** phimint uses pure crates.io dependencies — `cargo build` downloads
everything you need. You don't need to clone any other repository for most contributions.

## Finding Something to Work On

- **[Good First Issues](https://github.com/hibuka-labs/phimint/labels/good%20first%20issue)** — small, well-scoped tasks ideal for new contributors.
- **[Help Wanted](https://github.com/hibuka-labs/phimint/labels/help%20wanted)** — larger features the maintainers would love help with.
- **Got your own idea?** Open an issue first to discuss it before writing code.

## Scope

phimint is the **product shell** — TUI app, config, update checker, and the
system prompt that drives the coding loop. Framework capabilities belong in the
phi family crates:

| Change | Where it belongs |
|--------|------------------|
| Agent runtime / orchestration / sessions | [phi-agent](https://github.com/hibuka-labs/phi-agent) |
| Chat TUI widgets (transcript, composer, markdown) | [phi-tui](https://github.com/hibuka-labs/phi-tui) |
| LSP / repo map / search cores | [code-intel](https://github.com/hibuka-labs/code-intel) |
| Kernel tools (file, shell, context rotation) | [phi-kernel-tools](https://github.com/hibuka-labs/phi-kernel-tools) |
| Multi-agent runtime, memory | [agent-works](https://github.com/hibuka-labs/agent-works) |

If a PR moves product-shell logic into phimint that belongs upstream (or vice
versa), we will ask for a split. When in doubt, open an issue first.

## Before Submitting a PR

```bash
cargo fmt --check    # Formatting
cargo clippy --all-targets -- -D warnings   # Linting
cargo test           # Tests
```

All three must pass. CI runs them automatically on every PR, so save yourself a round-trip.

## Pull Request Checklist

1. Create a feature branch from `main`
2. Make your changes, with tests if applicable
3. Run the checks above (`fmt`, `clippy`, `test`)
4. Add an entry to `CHANGELOG.md` under `[Unreleased]`
5. Open a PR with a clear description of **what** and **why**

## Review Timeline

We review PRs within **48 hours**. Small, focused PRs (one logical change) get merged faster.

## Commit Style

- Present tense, imperative mood: "add feature" not "added feature"
- One logical change per commit
- Reference issues with `#123` when applicable

## Advanced: Working Across Crates

If your change requires modifying a sibling crate (phi-agent, phi-tui, etc.) simultaneously:

```bash
# Clone the dependency alongside phimint
cd ..
git clone https://github.com/hibuka-labs/phi-agent.git
cd phimint

# Temporarily add a path override in Cargo.toml (DO NOT COMMIT):
# [patch.crates-io]
# phi-agent = { path = "../phi-agent" }
```

Remove the override before committing.

## Questions?

Open a [GitHub Discussion](https://github.com/hibuka-labs/phimint/discussions) or an issue.
