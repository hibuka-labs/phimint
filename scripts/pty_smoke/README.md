# scripts/pty_smoke — shell PTY smoke suite

Gate asset for the `phi-agent-tui` shell (design Issue 3.2, PTY 门). Not part
of `cargo test` — PTY runs are slow and environment-sensitive. Position:
**步 4 / release gate**（发版前跑一遍；慢归慢，挡的是「屏幕上到底有没有」这一类
单元测试看不见的回归）。

## Usage

```bash
python3 scripts/pty_smoke/smoke.py            # all 11 scenarios
python3 scripts/pty_smoke/smoke.py 03 06      # selected ids
PHIMINT_BIN=$PWD/target/release/phimint python3 scripts/pty_smoke/smoke.py
```

`PHIMINT_BIN` may be relative — the harness resolves it to an absolute path
before spawning (the TUI is launched inside an isolated workspace, so a path
left relative would not be found there).

Exit code 0 = all green. Failures print a screen-dump path
(`/tmp/pty-smoke-<tag>-out/screen-*.txt`) — the dump is the CSI-replayed grid,
i.e. literally what was on screen, not the byte stream.

## Scenarios

| id | guards |
|---|---|
| 01 | hardware cursor sits on the composer (the IME-preedit bug class) + DSR round-trip + idle status |
| 02 | markdown rendering: heading / list / inline code+bold / long-line wrap |
| 03 | completion pickers: slash + mention, Esc policy duality (slash 恒抹 / mention 保文), no-match Enter submits verbatim |
| 04 | scrolling: PageUp/PageDown paging over a long reply, wheel tick, fresh output returns to the bottom |
| 05 | mouse selection + copy menu (SGR press/drag/release, right-click menu) + Ctrl+C-with-selection is copy, not quit-hint |
| 06 | approval gate: ask-mode write tool opens the popup; `y` + Enter approves and the turn completes |
| 07 | task panel: `spawn_agent` row while the child runs, parent completes, child conversation ran |
| 08 | fixed protocol: Esc layering, Ctrl+C interrupt settles with cancellation notice, Ctrl+L repaint erases foreign paint, unknown OSC query survives |
| 09 | periodic probes: UI keeps drawing during a slow stream; resize self-heal (shrink + restore) keeps chrome and composer |
| 10 | shallow-tier injection timing invariant (T16): a synthetic turn (child-report batch, background-task wake) is honored only when no turn is active — never injected into a running one |
| 11 | screenshot paste pipeline: Ctrl+V writes the chip row, the picture rides the wire request, an empty bracketed paste grabs the clipboard image all the same |

## The craft (each item earned its place)

- **DSR auto-answer** — the TUI asks `ESC[6n` at boot; a silent PTY wedges it.
  `Driver.drain` answers `\x1b[{rows};1R` and counts the queries.
- **TIOCSWINSZ before the first draw** — rows/cols must be set on the slave
  pre-exec or the first frame is sized wrong.
- **HOME isolation** — every scenario gets `/tmp/pty-smoke-<tag>-{home,ws,out}`;
  the real `~/.phimint` is never touched. Session logs land under the fixture
  HOME (`home/.phimint/sessions/<id>/session.log`) — read those for debugging.
- **CSI replay into a cell grid** — 字节流≠屏幕: absence claims ("the popup is
  gone") assert against the replayed grid, with East-Asian-width advance (W/F
  = 2 cells) so CJK lines land at the right columns. The grid also tracks the
  pen, which is how the hardware-cursor assertion works.
- **Slave-side `paint()`** — `spawn` keeps the slave fd so a test can write
  terminal output *behind the TUI's back* (exactly a misbehaving child's
  stderr); Ctrl+L must erase it. Writing this through the master would just be
  keyboard input.
- **MockLLM classification** — title generator and completion judge get canned
  answers first so they never eat the script; a spawned child's conversation is
  identified by scenario-registered `child_marks` matched against the **first
  user message only** (the parent's prompt mentions sub-agents and its
  continuation tool results quote the task text — whole-request matching
  misclassifies both).
- **Real product strings** — assertions bind to the strings the product actually
  paints ("Idle", "shift+tab", "Operation cancelled", "!! Approval required",
  "📋 copied N chars", …), never to guessed wording.

## Assertion gotchas (learned the hard way)

- **Loose predicates fire on the boot chrome.** The boot note contains "Allow
  Mouse Reporting" — a wait predicate of `contains("Allow")` matches it before
  the approval popup ever opens, and the keys then land in the composer. Wait
  on the exact popup title ("Approval required").
- **Transient notices are cleared by the next key.** "📋 copied N chars" from
  one copy lingers until another key event — to prove a *second* copy ran,
  change the selection size and assert the char count changed.
- **Approval is two-step.** Bare `y`/`a`/`n` only set the selection; the
  legend's "[Enter] confirm" is literal. Send `y` then Enter.
- **Esc policy is dual.** Slash Esc wipes the typed prefix (恒抹); mention Esc
  keeps it. Asserting the other way round is asserting a bug.
- **PageUp/PageDown are page steps** — over a long reply they need a bounded
  loop (`for _ in range(8)` until the sentinel row shows), not a single press.
- **Quit-settle ≠ "Idle".** After Ctrl+C a turn settles as "Operation
  cancelled" + "crunched for … - error"; don't wait for the idle status.

## The seam for a second consumer (产品 2)

`harness.py` is product-agnostic except two functions:

- `write_config(home, mock_port)` — one product's fixture config shape.
- `spawn_argv(binary, ws, cfg_path, session_id, approval)` — one product's CLI
  shape (note `spawn` post-processes `--base-url` to point at the mock).

A second consumer lifts `harness.py` + the scenario idioms and replaces exactly
those two functions; `MockLLM`, `Screen`, `Driver`, `World`, and the accounting
stay as-is. Scenario bodies that assert product *wording* (CopyTable strings,
banner lines) are product-side by definition — the shell-side twins assert the
mechanism (open/close, focus, protocol), not the copy.
