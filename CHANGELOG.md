# Changelog

All notable changes to phimint.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **The work has to compile before it is handed back.** A new `verify` tool
  runs the project's own compile-level check and reports the compiler's
  **real exit code** — spawned directly, never through a shell, so
  `cargo check 2>&1 | tail -40` cannot launder a failed build into a
  success. A gate then asks for the evidence at hand-back time: is there a
  green light *newer than the last edit*? If not, the hand-back is
  intercepted once, and after that it carries a visible marker instead of
  failing the run — a stubborn agent never kills the session. Prose and
  configuration never trigger it: only code that has to compile does. Which
  command runs is read off the project itself — its own config, then a slot
  it already fills in, then the language default — so nobody is asked to
  configure anything and the model is never allowed to pick the command it
  is graded by.

## [0.8.0] - 2026-11-09

### Added

- **Your model choice survives restarts.** `/model` used to reset on every
  launch; now the switch writes one top-level `model` selection pointer into
  config.json and the next boot serves the pick. The `main`, `lite` and
  `advanced` definitions never move — the pointer overrides `main` while it is
  there (an unconfigured lite or advanced follows the effective main — the
  pointer while one is set), and clearing it in `/model` deletes just the
  pointer. A pointer you placed by hand is kept
  and named. A `--model` or endpoint passed on the command line stays session
  policy and is never written to the file. Typed names nothing vouches for
  come back with a spelling warning.

- **Paste a screenshot with Ctrl+V.** The composer takes an image paste (the
  terminal keeps Cmd+V for text) and attaches it to your message. Whether the
  live model can actually see it is answered from the model registry's own
  fact — a model the registry has never heard of answers "unknown" rather than
  a guess.

- **The model says its own name** — the prompt names the live model on every
  turn, and "which model are you?" routes to a `current_model` answer that
  beats conversation history — weaker backends used to echo a model name from
  earlier in the conversation even after a switch.

- **Questions get a chat-about-this door.** When the agent asks with a picker,
  you can type a question back instead of choosing. Your words come back to
  the model verbatim, and the model answers them before carrying on.

- **Long shell output spills instead of vanishing.** Output that overflows the
  tool budget spills to the session's artifact store and is fetched exactly
  like a code-cell artifact, instead of ending in a truncation marker.

- **Your sent inputs come back on `Up`.** The composer keeps what you send and
  brings it back with `Up` / `Down` (your current draft cycles along), filtered
  to the workspace you are in. One file backs it —
  `~/.phimint/input-history.jsonl`, created 0600 inside a data directory
  created 0700.

- **You can select text with the mouse — character by character.** Press and
  drag over the transcript selects exactly what is under the pointer instead of
  whole lines; the right-click menu copies it. A wide CJK glyph selects whole
  (never half), a double click takes the word, a triple click the block, and
  dragging past the edge scrolls the view.

- **The plan block follows the tail.** When the agent updates its plan, the
  block moves to the end of the output instead of being replaced where it
  first appeared — a long session no longer scrolls the plan out of sight.
  Reading above the change does not move the screen.

- **The agent can look at a picture.** A new `read_image` tool takes a local
  path or an http(s) URL and puts the image on the wire. Oversized images are
  scaled to fit the gateway's request limit, and one that cannot fit is
  refused with the numbers that say why.

### Fixed

- **Two skills with the same name no longer shadow each other silently.** The
  duplicate is reported instead of one quietly winning.

- **`/resume` writes to the right session.** The rotation archive, the
  notes, the code journal and the captures were built once, with the launch
  session's id copied into them, so a mid-session switch kept writing the
  abandoned session's directories — the resumed session's audit stream
  landed in the bootstrap session's tree. The stores now share one
  re-pointable session scope, re-pointed once per session.

- **`agent-works` compiles with the `multi_agent` feature alone.** A file
  split left the `Arc` import gated on the `memory` and `skill` features
  only, so the multi-agent build paths had no `Arc` in scope and the crate
  failed to compile with `multi_agent` enabled but the other two disabled.

### Changed

- **The canvas surface speaks English** — the status line, the option labels
  and the pick marker the model reads (`(canvas pick)`) are English now; the
  design-canvas skill's decision sections follow.

- **The task panel draws a rule, not a box** — the frame keeps its top rule
  — the row count and the close hint still ride it — and drops the sides
  and the bottom, so the rows read as a section rather than a box.

- **The live phase word and its hint wear different colors** — the process
  row's phase word (`thinking...`) takes the brand's bright warm; the
  parenthetical hint beside it (`(Ctrl+C cancel)`) drops to the gray hint
  slot. The mode badge keeps its own warm step.

- **Task names get the room they need.** The name column widened from 16 to
  28 columns and the files column from 8 to 12, so a long task name shows
  whole instead of being ellipsized. The squeeze order is unchanged.

- **The tips line tells the truth about newlines** — the newline hint is Ctrl+J
  (Shift+Enter never arrived in the terminal), and the tips mention Ctrl+V for
  screenshot pastes.

- **Duplicate skill names are no longer silent.** When two skill directories
  answer to the same name and their content diverged, the startup log says
  which one won.

## [0.7.1] — 2026-10-31

> 0.7.0 was yanked: its Windows build did not compile (see below). 0.7.1
> carries the same features plus the fix.

### Fixed

- **Windows build** — the terminal-restore mechanism used POSIX `termios`
  and signal calls directly, but `libc` is a Unix-only dependency, so the
  Windows binary failed to compile. The repair now lives behind a
  `cfg(unix)` gate and Windows gets no-ops for the same two entry points:
  there is no line discipline to restore there, and the API is unchanged on
  every platform.

  CI now compiles the crate for the Windows target on every push, so a
  platform-only break is caught before a release reaches crates.io — the
  5-platform build used to run *after* the publish.

## [0.7.0] — 2026-10-31

### Added

- **CodeAct (`code_cell`)** — the agent runs code in a resident Python session.
  One call is one step of cells sharing one session, so state (variables,
  imports, functions) survives across calls. Mechanical multi-step work —
  bulk edits, renaming, data wrangling — now rides a short cell script
  instead of a long chain of single-purpose tool calls. The engine is behind
  a compile-time kill switch: `--no-default-features` drops it from the
  binary entirely.
- **Desktop capability pack (`desk.*`)** — the agent can look at and move the
  pointer on a real desktop. The pack is mounted at the assembly point, never
  inside the engine, and the model names the device methods it wants.
- **Device consent, posture B** — *looking* at the desktop and moving the
  pointer never asks; changing the machine asks every time, through the same
  approval gate the tool path already uses. The consent line says who is
  holding things up and that the page has no button to press, so a model
  never goes hunting for one.
- **Interaction channel** — the `ask_user` picker and the consent gate share
  one live channel, so a question asked mid-run and a consent request both
  land in the same popup with the same ledger.
- **Terminal restore** — if phimint dies without its normal exit path, the
  terminal is put back (raw mode, alternate screen, mouse reporting,
  bracketed paste) instead of leaving the shell in a broken state.

### Changed

- **Browser tool face per turn** — the browser suite is loaded per turn by
  usage scenario: the design-canvas self-check only ever needs the canvas
  group, while full browser automation takes the whole suite. The menu you
  see now matches what the run actually needs.
- **`repo_map` carries file sizes** — the outline header states each file's
  line count, so "read the whole file or just these symbols" is decided from
  one call.
- **Tool results can carry pictures** — a tool result that hands the model
  something to *look at* (a screenshot, a rendered diagram) delivers the image
  beside its text, and the picture rides the persisted transcript, so a
  session resumed later still hands the model what it was shown.

### Fixed

- **Design-canvas consent wording** — the awaiting-consent line no longer
  reads like a task, which had sent a live model hunting for a button that
  is not on the page.
- **Compare view** — the design-canvas compare link appears as soon as there
  is a version behind the current one, instead of only when two versions are
  already on screen.

## [0.6.0] — 2026-10-24

### Added

- **Design canvas** — the agent shows you design options in your browser and
  you click to pick. `canvas_open` / `canvas_push` / `canvas_close` push 2–4
  candidates as a screen (layout mockups, option sets, comparisons); your pick
  comes back as a real user turn. The browser opens only after a one-time
  consent — never a standing grant — and a `y` that lands after the canvas
  closed recovers (rebind + the new URL) instead of printing a dead link.
  Busy clicks queue at the turn boundary and ack as queued with their labels;
  a canvas move to a new port is announced in the transcript. While a screen
  awaits your pick the run parks: the agent never decides your design choices
  for you.
- **`ask_user`** — the agent asks you a concept question right in the terminal
  picker: 2–4 candidates as label + note rows, one shot per question, Esc =
  skip is an answer. Approval-wired and pause-aware (your thinking time never
  burns a tool budget).
- **design-canvas skill** — design-to-page laws for turning a settled mock
  into a page: Ant Design default floor (a user-named stack wins and gets
  named in the decision record), mechanical translation of the mock with no
  embellishment, theme tokens on every visual surface, npm mirror retries.

### Added (UI)

- Upgrade notices are committed into the transcript — the status slot dies
  with the alt-screen and the hint used to evaporate.
- Startup notice when PATH holds more than one `phimint` binary, marking the
  running one (a stale `~/.cargo/bin` shadowing `~/.local/bin` made
  `--version` contradict the installer).

### Changed

- Family dependencies: `phi-agent` 0.19.1, `phi-kernel-tools` 0.11.0 (file
  tools expand `~`), `phi-tools` 0.7.0 (lazy browser — the agent drives its
  own Chrome to look at what it just showed you), new `phi-canvas` 0.1.0;
  the family aligns on `agent-base` 0.11.0 (per-call timeout budgets).

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
