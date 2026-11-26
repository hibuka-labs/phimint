# phimint — user manual

The complete reference: commands, keys, approvals, the tool surface,
session files and the internal layout. Install and first run live in the
[README](../README.md) — [中文版](USAGE_CN.md)

## Command line

```
phimint [OPTIONS] [COMMAND]

Commands:
  update             upgrade to the latest release (self-replaces standalone
                     installs; brew/npm/cargo/scoop installs get the
                     matching upgrade command instead). `phimint update --check`
                     reports only.

  uninstall          remove phimint and the data it left behind. The binary goes
                     back to whichever channel installed it; then ~/.phimint/
                     (API key, sessions, history, notes) is deleted and the
                     PATH entry the installer added is dropped — the user-PATH
                     entry on Windows, the marked block in your shell profile on
                     Unix. --keep-data leaves ~/.phimint/ alone. --yes skips
                     the prompt.

  init               write the config skeleton without starting the TUI. The
                     installer runs `phimint init --if-missing`; an existing
                     config is never touched.

  config             inspect or edit the config file without starting the TUI.
                     `config show` prints what phimint resolved with the API key
                     masked; `config set <key> <value>` writes one member
                     (api_key, base_url, protocol, main). For api_key, pass `-`
                     to read the secret from stdin instead of the command line.

  -w, --workspace <PATH>       workspace directory (default: current directory)
      --model <NAME>           main model for this run only (overrides the
                              config.json `model` pointer and `main`; never
                              written back to the file)
      --lite-model <NAME>      lite model (overrides config.json)
      --advanced-model <NAME>  advanced model (overrides config.json)
      --base-url <URL>         API base URL (overrides config.json)
      --api-key <KEY>          API key (overrides config.json)
      --protocol <PROTO>       API protocol (default: inferred from model)
      --config <PATH>          config file path (default: ~/.phimint/config.json)
      --approval <MODE>        auto (default) / ask / deny
      --shell-timeout-ms <N>   shell command timeout (default 120000)
      --session <ID>           session ID (default: auto; reuse to append turns)
      --resume                 pick a previous session interactively
      --log-level <LEVEL>      session.log level (default info)
      --color-scheme <S>       banner colors: auto (default) / dark / light
      --banner <on|off>        startup banner (default on)
      --thinking-budget <N>    reasoning token budget (default 8192)
      --reasoning-effort <E>   none/low/medium/high/xhigh (default medium)
      --token-budget <N>       context-window token budget (default 210000)
      --session-retention-days <N>  days to keep session history (default 30)
      --no-update-check        skip the startup update check
```

## Interface & interaction

- **Composer & transcript** — fixed bottom composer (multi-line, bracketed paste, CJK double-width aware); output streams token-by-token with markdown rendering. Type `@` for file-path completion, `/` for the skill picker.
- **Keys**

  | Key | Action |
  |---|---|
  | `Enter` | send (also jumps to the newest lines) |
  | `Ctrl+J` | newline (works on every terminal; `Shift+Enter` too where the Kitty keyboard protocol is reported) |
  | `Up` / `Down` | recall sent inputs (fish-style: your current draft cycles with them) |
  | `Alt+Up` / `Alt+Down` | move the cursor inside a multi-line draft |
  | `Shift+Tab` | toggle approval mode (auto ⇄ ask) |
  | `Ctrl+O` | expand/collapse thinking blocks and long pasted messages |
  | `Ctrl+V` | paste a clipboard image (screenshot) — a model that cannot see images is refused |
  | `Ctrl+Y` | copy the last reply |
  | `Ctrl+C` | context-sensitive: copy selection → cancel run (a draft that produced nothing comes back to the composer) → clear the composer draft → quit (press twice) |
  | `Ctrl+D` | quit |
  | `Esc` | clear selection, else clear the composer |
  | `PageUp` / `PageDown` | scroll the transcript |
  | `Ctrl+End` | jump to the newest lines |

- **Mouse** — wheel scrolls; left-drag selects lines; right-click opens a copy menu.
- **Terminal.app** — turn on **View → Allow Mouse Reporting** or the wheel will not scroll the transcript; without it, page with `fn+Up` / `fn+Down`. This terminal also skips the Kitty keyboard protocol, so it never sees `Cmd+C` as copy — select first, then press `Ctrl+C`, or use the right-click menu.
- **Approvals**

  | Mode | Behavior |
  |---|---|
  | `auto` | everything allowed; write-capable sub-agents write freely too |
  | `ask` | each write prompts: `y` allow once / `a` allow always / `n` deny |
  | `deny` | read-only; all writes rejected |

  `Shift+Tab` toggles `auto` ⇄ `ask` mid-session (the status bar shows the
  current mode); `--approval` picks the starting mode.

## Tool surface

| Tool | What it does |
|---|---|
| `code_cell` | run code cells in a resident Python session — state (variables, imports, functions) survives across calls; the fast lane for bulk edits, renaming and data wrangling |
| `read_file` / `write_file` / `edit_file` / `list_files` | file I/O and surgical edits (`edit_file` is atomic with 4-level matching; phi-kernel-tools) |
| `read_image` | read an image the model can actually see — paste a screenshot with Ctrl+V and the agent looks at it (or point it at a file; phi-kernel-tools) |
| `execute_command` | shell (builds/tests/anything; streamed line-by-line, timeout + cancel kills the process group; output over the tool budget spills to a file the agent can read back) |
| `search_content` | ripgrep content search (regex) |
| `repo_map` | tree-sitter symbol/structure index (multi-language, pull-based context) |
| `diagnostics` | LSP diagnostics (fast loop without recompiling; degrades to shell) |
| `update_plan` | structured checklist for complex tasks (display-only, Codex-style; agent-base) |
| `current_model` | the model serving this session (the agent calls it before answering "which model are you") |
| `spawn_agent` / `send_message` / `wait_agent` / `list_agents` / `close_agent` | sub-agent primitives (agent-works; preferred pattern: spawn, end turn, await push) |
| `add_tool` | the agent writes and registers a tool for you (root-only; full confirmation card before anything lands) |
| `canvas_open` / `canvas_push` / `canvas_close` | design canvas — browser screens with clickable options; one-time consent to open the browser ([`phi-canvas`](https://github.com/hibuka-labs/phi-canvas)) |
| `ask_user` | terminal picker for concept picks (2–4 candidates, one shot, Esc = skip; you can type a question back instead of picking) |
| `/connect` · `/tools` · `plugin add` / `plugin remove` | external tool surfaces: MCP servers, script tools, wasm plugin packs (approval ledger per tool, fail-closed) |

## How it works

phimint is a consumer of the [`phi-agent`](https://docs.phiagent.dev/) framework: the agent loop, ReAct, approvals, session event stream and guards all come from the framework (one facade dependency). phimint brings the product shell — system prompt, tool registration, approval wiring, TUI.

- **Push-based fan-in** (agent-works) — sub-agent reports are held on completion and injected as a new turn the instant the main agent's turn ends; progress is display-only (never wakes or interrupts). A task over 10 minutes is hard-stopped with an Error result, so a hung child can always wake the parent. Sub-agents are read-only by default — `write_file` / `edit_file` / `execute_command` stay off their tool surface until write capability is requested for that spawn (`tools: "write"`, or a `coder`/`tester` preset). Write-capable children work disjoint file sets under a file lock (`file locked by <agent>` on collision), and in `ask` every child write is confirmed in a popup.
- **TUI architecture** — the shell in `phi-agent-tui` runs a background task per agent turn pushing `RuntimeEvent`s over an mpsc channel; the main loop feeds events into the `App` state machine, polls keys and repaints. `src/shell/wire.rs` fills every slot the shell offers, so the wording and the policy stay on this side. Input and events only meet through channels — no shared mutable state, zero intrusion into the agent loop.
- **Task panel** — the shell's `task_panel.rs` books sub-agent lifecycle (appears on spawn, flips on done, recycled 3s after completion, deferred while the root is busy or you're looking at the panel). Each sub-agent streams into its own buffer so interleaved output never cuts lines.
- **LSP** — `code-intel` lazily starts one server per language, syncs `didOpen`/`didChange`/`didSave` with cached `publishDiagnostics`, pulled by the `diagnostics` tool. The registry picks the server; code-intel knows nothing about phimint.
- **Guards** (framework-provided, product-tuned) — reasoning-only spin: two empty turns in a row injects an "act now" nudge and disables thinking; the end-of-turn judge fails open; `MaxTurnsNudgeMiddleware` forces a final answer in the last 3 turns of a 256-turn budget.

## Sessions & observability

Every run lands a session directory under `~/.phimint/sessions/<id>/`:

| File | Contents |
|---|---|
| `session.log` | tracing log (via log-core) |
| `turn_NNN.jsonl` | structured per-turn event stream (tool calls, text deltas, reasoning …) |
| `frames.txt` | flip-book of TUI frames (colors stripped, layout kept) for offline UI replay |
| `perf.log` | per-frame render-time CSV (performance regression hunting) |
| `session_metrics.json` | per-turn token usage and session totals (phi-telemetry) |

Re-running with `--session <id>` appends new turns to the same directory; `--resume` picks one interactively.

Composer history lives in `~/.phimint/input-history.jsonl`, next to the
`sessions/` directory (owner-only, one global file; `Up` / `Down` recall it
per workspace).

## Project structure

```
phimint/
├── src/
│   ├── main.rs        # CLI entry (clap), assembly, banner color detection
│   ├── lib.rs         # module root — the binary is a thin shell over it
│   ├── cli.rs         # the CLI surface: every flag and subcommand
│   ├── command.rs     # shell command parsing + risk classification
│   ├── agent.rs       # system prompt + tool registration + multi-agent/guard/approval wiring
│   ├── agent/         # agent assembly: build.rs, gate.rs, prompt.rs, spawn_resolve.rs
│   ├── approval.rs    # two layers: ApprovalPolicy gate + decision handler (TUI queue)
│   ├── banner.rs      # startup banner (light + dark palettes)
│   ├── model/         # config, tiers, routing, persistence, the first-run gate
│   ├── shell/         # the TUI shell: wire.rs fills phi-agent-tui's slots
│   ├── tools/         # application tool shells (cores live in code-intel / phi-kernel-tools)
│   │   ├── diagnostics.rs   # LSP diagnostics tool
│   │   ├── repomap.rs       # repo_map tool
│   │   ├── ripgrep.rs       # search_content tool
│   ├── external/      # the external tool socket: MCP servers, script tools, wasm plugins
│   ├── session/       # session store, resume, retention
│   ├── ops/           # uninstall, terminal restore, install-source bookkeeping
│   └── update/        # update checker + release manifest
└── Cargo.toml
```

## Run from source
Requires a Rust toolchain (edition 2024). Optional: language servers on `PATH`
(rust-analyzer, typescript-language-server, clangd) for the LSP fast loop;
`cargo` / `npm` / `make` and friends for whatever project you point it at.
Everything else comes from crates.io — `cargo build` needs no sibling checkouts.
```bash
cargo run                                # run straight from the source tree
cargo run -- -w ../some-project          # point at another workspace
```

## Development

```bash
cargo build            # compile
cargo test             # all tests (unit tests live in standalone *_tests.rs files)
cargo clippy           # lint
```

Tests are split from business code (module-separated tests): `app.rs`, `render.rs` and friends hold logic only; tests sit in `app_tests.rs` etc., `#[cfg(test)]`-isolated from production builds but still inside the crate, so `pub(crate)` members are reachable.

Dependencies are pure crates.io version references. To hack on a sibling crate (phi-agent, phi-tui, …) at the same time, add an uncommitted `[patch.crates-io]` path override (see [CONTRIBUTING.md](CONTRIBUTING.md)). Design notes live in `notes/` (local, not committed); `docs/` holds public-facing documentation and assets.

The README demo GIF is rebuilt from a recorded session: `cargo test frame_reel -- --ignored` writes ANSI frames to `target/reel/`, then `python3 scripts/readme_reel.py` renders them to `docs/assets/readme-demo.gif`.
