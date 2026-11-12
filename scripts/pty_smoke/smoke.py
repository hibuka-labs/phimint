#!/usr/bin/env python3
"""Shell PTY smoke suite — the gate asset (design Issue 3.2, PTY 门).

Covers the mandated surfaces (markdown / completion / scroll / selection /
approval / task panel / hardware cursor) plus the shell's fixed protocol
(Ctrl+C interrupt discipline, Ctrl+L repaint, Esc layering, DSR round-trip),
the periodic probes (resize self-heal, liveness during a slow stream), and
the shallow-tier injection timing invariant (a synthetic turn is injected
only when no turn is active — T16).

Not in `cargo test` — PTY runs are slow and environment-sensitive. Position:
步 4 / release gate. Usage:
    python3 scripts/pty_smoke/smoke.py            # all scenarios
    python3 scripts/pty_smoke/smoke.py 03 06      # selected ids
Env: PHIMINT_BIN overrides the binary (default target/debug/phimint).
"""

import json
import os
import re
import shutil
import struct
import subprocess
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (  # noqa: E402
    Driver, MockLLM, World, boot_ok, child_marks_contract_selftest, expect,
    record, spawn, summary, summary_mark_contract_selftest,
)
from model_route_smoke import (  # noqa: E402
    s13_spawn_vs_model_hotswap, s14_judge_stays_lite, s15_wire_invariant,
)

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
# The binary path must be absolute: `spawn` chdirs into an isolated workspace
# before execve, so a relative PHIMINT_BIN would resolve against the workspace
# and fail with FileNotFoundError at boot (every scenario then reports "boot"
# failed). Resolve against the caller's cwd, not the isolated one.
BIN = os.path.abspath(
    os.environ.get("PHIMINT_BIN", os.path.join(ROOT, "target", "debug", "phimint"))
)

IDLE = "Idle"          # status line when no turn runs
MODE_HINT = "shift+tab"  # status-bar mode badge


def new_session(tag, mock, approval=None, extra_env=None):
    world = World(tag)
    pid, fd, slave = spawn(BIN, world, mock, f"smoke-{tag}", approval,
                           extra_env=extra_env)
    drv = Driver(pid, fd, slave)
    ok = boot_ok(drv)
    return world, drv, ok


def finish(drv, world, tag):
    drv.dump(world, tag)
    drv.quit()


def submit_and_wait_reply(drv, text, done_mark, timeout=12.0):
    drv.type(text)
    drv.key(drv.ENTER)
    return drv.wait_for(lambda s: s.contains(done_mark), timeout)


def grid_col_of(scr, r, needle):
    """Grid column where `needle` starts on screen row `r` — grid cells, so a
    wide CJK glyph advances two. Mouse coordinates are grid columns, while
    the selection's text columns are pane-relative; anchoring every press at
    `grid_col_of(...) + offset` keeps the two in step."""
    text, cols = "", []
    for c, cell in enumerate(scr.grid[r]):
        if cell == "":
            continue  # wide-glyph continuation cell
        text += cell
        cols.append(c)
    i = text.find(needle)
    return None if i < 0 else cols[i]


# ── 01 hardware cursor + DSR round-trip ─────────────────────────────────

def s01_hardware_cursor():
    """The cursor must sit on the composer while typing — never stranded in
    the status bar (the IME-preedit bug class). DSR must be answered (any
    completed draw proves it; we also count the queries)."""
    mock = MockLLM()
    world, drv, ok = new_session("01", mock)
    if not expect("01", "boot draws through DSR", ok, drv.screen.text()[-400:]):
        drv.quit()
        return
    drv.type("hello cursor")
    drv.wait_for(lambda s: s.contains("hello cursor"), 3.0)
    row = drv.screen.row_of("hello cursor")
    crow, _ = drv.screen.cursor()
    expect("01", "hardware cursor sits on the composer line",
           row is not None and crow == row,
           f"cursor row={crow}, composer row={row}")
    expect("01", "boot settles to the idle status", drv.screen.contains(IDLE),
           drv.screen.text()[-300:])
    expect("01", "DSR queries were answered", drv.dsr_answered >= 1,
           f"dsr_answered={drv.dsr_answered}")
    finish(drv, world, "01")


# ── 02 markdown rendering ───────────────────────────────────────────────

def s02_markdown():
    """A markdown reply renders its structure as transcript rows: heading
    text, list items, inline code/bold words all land readable on screen."""
    mock = MockLLM()
    mock.set_script([{"text":
        "# Heading-MARK\n\n"
        "- item-alpha\n"
        "- item-beta\n\n"
        "Inline **bold-word** and `code-word` plus a "
        + "very-long-line-word " * 8 + "END-MARK"}])
    world, drv, ok = new_session("02", mock)
    if not expect("02", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    ok = submit_and_wait_reply(drv, "render this", "END-MARK", 15.0)
    if not expect("02", "reply arrives", ok, drv.dump(world, "02-wait")):
        finish(drv, world, "02")
        return
    s = drv.screen
    expect("02", "heading text renders", s.contains("Heading-MARK"), s.text()[-500:])
    expect("02", "list items render", s.contains("item-alpha") and s.contains("item-beta"),
           s.text()[-500:])
    expect("02", "inline words land", s.contains("bold-word") and s.contains("code-word"),
           s.text()[-500:])
    expect("02", "long line wraps (no clipping)", s.contains("END-MARK"), s.text()[-500:])
    finish(drv, world, "02")


# ── 03 completion (slash + mention pickers) ─────────────────────────────

def s03_completion():
    """`/` opens the command picker; the Esc policy duality holds at PTY
    level — slash Esc wipes the typed prefix, mention Esc keeps it — and
    Enter with no match submits the raw typed command (the two accident
    red-lines' PTY twins)."""
    mock = MockLLM()
    mock.set_script([
        {"text": "submitted-ok-1"},
        {"text": "submitted-ok-2"},
    ])
    world, drv, ok = new_session("03", mock)
    if not expect("03", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    with open(os.path.join(world.ws, "fixture-file.txt"), "w") as fh:
        fh.write("fixture\n")
    s = drv.screen
    drv.type("/")
    drv.wait_for(lambda s: s.row_of("resume") is not None, 3.0)
    expect("03", "slash opens the picker", s.row_of("resume") is not None, s.text()[-500:])
    # Slash Esc: closes the picker AND wipes the typed prefix (policy 恒抹).
    drv.type("mo")
    drv.drain(0.4)
    drv.key(drv.ESC)
    drv.drain(0.6)
    expect("03", "slash Esc closes picker and wipes the prefix",
           s.row_of("resume") is None and not s.contains("no matching")
           and "/mo" not in s.text(), s.text()[-500:])
    # Enter with no match submits the typed command verbatim.
    for _ in range(4):
        drv.key(b"\x7f")  # backspace: clear composer
    drv.type("/zzz-nope")
    drv.key(drv.ENTER)
    ok = drv.wait_for(lambda s: s.contains("submitted-ok-1"), 12.0)
    expect("03", "no-match Enter submits the typed command",
           ok and drv.screen.contains("/zzz-nope"), drv.screen.text()[-500:])
    # Mention Esc: closes the picker and KEEPS the typed prefix (policy 对偶).
    for _ in range(10):
        drv.key(b"\x7f")
    drv.type("@fixt")
    ok = drv.wait_for(lambda s: s.contains("fixture-file"), 3.0)
    drv.key(drv.ESC)
    drv.drain(0.6)
    expect("03", "mention Esc closes picker keeping the prefix",
           ok and s.contains("@fixt") and not s.contains("fixture-file"),
           s.text()[-500:])
    finish(drv, world, "03")


# ── 04 scrolling ────────────────────────────────────────────────────────

def s04_scroll():
    """A long reply overflows the viewport; PageUp reveals the head, PageUp/
    PageDown paging works, wheel ticks scroll one line, and fresh output
    lands back at the bottom."""
    n = 60
    body = "\n".join(f"LINE-{i:03d}" for i in range(n))
    mock = MockLLM()
    mock.set_script([{"text": body + "\nTAIL-MARK"}, {"text": "fresh-FRESH"}])
    world, drv, ok = new_session("04", mock)
    if not expect("04", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    ok = submit_and_wait_reply(drv, "long reply", "TAIL-MARK", 15.0)
    if not expect("04", "long reply arrives", ok, drv.dump(world, "04-wait")):
        finish(drv, world, "04")
        return
    s = drv.screen
    expect("04", "head is scrolled off", s.row_of("LINE-000") is None, s.text()[-300:])
    expect("04", "tail is visible", s.contains("TAIL-MARK"), s.text()[-300:])
    for _ in range(8):
        drv.key(drv.PG_UP)
        drv.drain(0.3)
        if s.contains("LINE-000"):
            break
    expect("04", "PageUp reveals the head", s.contains("LINE-000"), s.text()[-300:])
    drv.wheel(up=False, col=20, row=10)  # wheel down: back toward the bottom
    drv.drain(0.5)
    for _ in range(8):  # PageDown is a page step — loop until the tail returns
        drv.key(drv.PG_DOWN)
        drv.drain(0.3)
        if s.contains("TAIL-MARK"):
            break
    expect("04", "PageDown returns to the bottom", s.contains("TAIL-MARK"),
           s.text()[-300:])
    # New output returns the viewport to the bottom.
    ok = submit_and_wait_reply(drv, "more", "fresh-FRESH", 12.0)
    expect("04", "fresh output lands at the bottom",
           ok and drv.screen.contains("fresh-FRESH"), drv.screen.text()[-300:])
    finish(drv, world, "04")


# ── 05 mouse selection + copy menu ──────────────────────────────────────

def s05_selection():
    """Left-drag selects exactly the dragged cells (character-precise: the
    first row cuts at the anchor column, the last at the head column);
    right-click opens the copy menu; activating it closes the menu. Ctrl+C
    with a selection copies instead of interrupting."""
    mock = MockLLM()
    mock.set_script([{"text": "select-alpha\nselect-beta\nselect-gamma"}])
    world, drv, ok = new_session("05", mock)
    if not expect("05", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    ok = submit_and_wait_reply(drv, "some text", "select-gamma", 12.0)
    if not expect("05", "reply arrives", ok, drv.dump(world, "05-wait")):
        finish(drv, world, "05")
        return
    s = drv.screen
    r = s.row_of("select-alpha")
    if r is None:
        expect("05", "reply rows on screen", False, s.text()[-500:])
        finish(drv, world, "05")
        return
    x0 = grid_col_of(s, r, "select-alpha")
    # Drag from text col 5 of the first row past the second row's end (SGR
    # press, drag, release — 0-based grid): "select-alpha"[5:] + whole
    # "select-beta" = "t-alpha\nselect-beta" = 19 chars.
    drv.mouse(0, x0 + 5, r, press=True)
    drv.mouse(32, x0 + 25, r + 1, press=True)
    drv.mouse(0, x0 + 25, r + 1, press=False)
    drv.drain(0.5)
    drv.mouse(2, x0 + 12, r, press=True)  # right-click on the selection
    drv.mouse(2, x0 + 12, r, press=False)
    drv.drain(0.6)
    menu_up = s.contains("Copy")
    expect("05", "right-click opens the copy menu", menu_up, s.text()[-500:])
    first = None
    if menu_up:
        drv.key(drv.ENTER)  # menu Enter copies and closes
        drv.drain(0.6)
        expect("05", "menu Enter copies and closes",
               not s.contains("Copy"), s.text()[-500:])
        m = re.search(r"copied (\d+) chars", s.text())
        first = m.group(1) if m else None
    expect("05", "menu copy is the char-precise slice (19 chars)",
           first == "19", f"copied={first} screen={s.text()[-300:]}")
    # Ctrl+C with a selection copies — it never falls through to the quit hint.
    # Re-select a DIFFERENT, shorter span ("select-alpha"[5:] = 7 chars): the
    # changed count proves the copy path ran again rather than the stale
    # notice lingering.
    drv.mouse(0, x0 + 5, r, press=True)
    drv.mouse(32, x0 + 25, r, press=True)
    drv.mouse(0, x0 + 25, r, press=False)
    drv.drain(0.4)
    drv.key(drv.CTRL_C)
    drv.drain(0.6)
    m = re.search(r"copied (\d+) chars", s.text())
    expect("05", "Ctrl+C with selection is copy, not interrupt",
           m is not None and m.group(1) == "7" and not s.contains("Ctrl+C again"),
           s.text()[-300:])
    finish(drv, world, "05")


# ── 06 approval gate ────────────────────────────────────────────────────

def s06_approval():
    """In ask mode a write tool call opens the approval popup; `y` approves
    and the turn continues to completion."""
    mock = MockLLM()
    mock.set_script([
        {"tool_call": {"name": "write_file", "args": {"path": "smoke-06.txt", "content": "hi"}}},
        {"text": "approved-DONE"},
    ])
    world, drv, ok = new_session("06", mock, approval="ask")
    if not expect("06", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    drv.type("write the file")
    drv.key(drv.ENTER)
    # Popup title is the exact signal — a loose "allow" match hits the boot
    # note ("View > Allow Mouse Reporting") and fires before the popup opens.
    ok = drv.wait_for(lambda s: s.contains("Approval required"), 12.0)
    if not expect("06", "approval popup opens", ok, drv.dump(world, "06-popup")):
        finish(drv, world, "06")
        return
    drv.key(b"y")      # select allow-once …
    drv.drain(0.3)
    drv.key(drv.ENTER)  # … and confirm (popup: "[Enter] confirm")
    ok = drv.wait_for(lambda s: s.contains("approved-DONE"), 12.0)
    expect("06", "y+Enter approves and the turn completes", ok,
           drv.dump(world, "06-after"))
    finish(drv, world, "06")


# ── 07 task panel (multi-task) ──────────────────────────────────────────

def s07_task_panel():
    """`spawn_agent` puts a row on the multi-task panel while the child runs;
    the row survives to the child's completion."""
    mock = MockLLM()
    mock.child_marks = ["panel-row-MARK"]
    mock.set_script([
        {"tool_call": {"name": "spawn_agent",
                       "args": {"task": "panel-row-MARK", "task_name": "panel-row"}}},
        {"text": "spawned-DONE"},
    ])
    world, drv, ok = new_session("07", mock)
    if not expect("07", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    drv.type("delegate it")
    drv.key(drv.ENTER)
    ok = drv.wait_for(lambda s: s.contains("panel-row-MARK"), 15.0)
    if not expect("07", "task panel shows the child row", ok,
                  drv.dump(world, "07-panel")):
        finish(drv, world, "07")
        return
    ok = drv.wait_for(lambda s: s.contains("spawned-DONE"), 15.0)
    expect("07", "parent turn completes after the spawn", ok,
           drv.dump(world, "07-after"))
    expect("07", "child conversation ran", len(mock.trace_of("child")) >= 1,
           f"trace={mock.trace}")
    finish(drv, world, "07")


# ── 08 fixed protocol: Esc layering / Ctrl+C / Ctrl+L / OSC safety ─────

def s08_fixed_protocol():
    """The shell's fixed (non-configurable) protocol surface."""
    mock = MockLLM()
    mock.set_script([{"chunks": ["chunk-%d " % i for i in range(20)], "gap": 0.4}])
    world, drv, ok = new_session("08", mock)
    if not expect("08", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    s = drv.screen
    # Esc layering: the picker closes on Esc; slash Esc wipes the prefix.
    drv.type("/")
    drv.wait_for(lambda s: s.row_of("resume") is not None, 3.0)
    drv.type("mo")
    drv.drain(0.3)
    drv.key(drv.ESC)
    drv.drain(0.5)
    expect("08", "Esc closes the layer (slash wipes the prefix)",
           s.row_of("resume") is None and not s.contains("no matching")
           and "/mo" not in s.text(), s.text()[-500:])
    # Ctrl+C interrupt discipline: a slow stream stops mid-turn; the turn
    # settles with the cancellation notice instead of running on.
    drv.type("slow turn")
    drv.key(drv.ENTER)
    drv.wait_for(lambda s: s.contains("chunk-0"), 8.0)
    drv.key(drv.CTRL_C)
    # The cancellation marker row (batch 2 renamed it: the old generic
    # "Operation cancelled" line became the muted "⏹ cancelled" marker that
    # belongs to RunCancelled).
    ok = drv.wait_for(lambda s: s.contains("⏹ cancelled"), 8.0)
    expect("08", "Ctrl+C interrupts a running turn (cancellation lands)", ok,
           drv.dump(world, "08-interrupt"))
    ok = drv.wait_for(lambda s: s.contains("crunched for"), 5.0)
    expect("08", "interrupted turn settles", ok, drv.dump(world, "08-settle"))
    # Ctrl+L repaint: a foreign painter (slave-side output, the child-stderr
    # corruption class) smears the screen; repaint must erase the smear.
    drv.type("kept-text")
    drv.drain(0.3)
    drv.paint(b"\x1b[2;1HXXX-PAINT-MARK")
    drv.drain(0.4)
    painted = s.contains("XXX-PAINT-MARK")
    drv.key(drv.CTRL_L)
    drv.drain(0.8)
    expect("08", "Ctrl+L repaint erases foreign paint",
           painted and not s.contains("XXX-PAINT-MARK") and s.contains("kept-text"),
           s.text()[-500:])
    # Unknown OSC query: must not wedge or corrupt the screen.
    drv._write(b"\x1b]10;?\x07")
    drv.drain(0.5)
    expect("08", "unknown OSC query leaves the screen intact",
           s.contains("kept-text") and s.contains(MODE_HINT), s.text()[-300:])
    finish(drv, world, "08")


# ── 09 periodic probes: resize self-heal + liveness under slow stream ──

def s09_periodic_probes():
    """1 Hz size/raw-mode self-heal and UI_TICK liveness — the periodic
    probes the design marks as PTY-smoke priorities (§7.4)."""
    mock = MockLLM()
    mock.set_script([{"chunks": ["tick-%d " % i for i in range(12)], "gap": 0.5}])
    world, drv, ok = new_session("09", mock)
    if not expect("09", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    drv.type("slow stream")
    drv.key(drv.ENTER)
    drv.wait_for(lambda s: s.contains("tick-0"), 8.0)
    a = drv.screen.text()
    drv.drain(1.2)
    b = drv.screen.text()
    expect("09", "UI keeps drawing during a slow stream", a != b, "")
    ok = drv.wait_for(lambda s: s.contains("tick-11"), 15.0)
    expect("09", "stream completes", ok, drv.dump(world, "09-stream"))
    # Resize self-heal: shrink and grow; chrome must reflow and stay usable.
    drv.resize(30, 100)
    drv.drain(1.6)
    expect("09", "shrunk resize keeps chrome", drv.screen.contains(MODE_HINT),
           drv.screen.text()[-300:])
    drv.type("after-resize")
    drv.drain(0.5)
    expect("09", "composer works after resize",
           drv.screen.contains("after-resize"), drv.screen.text()[-300:])
    drv.resize(40, 120)
    drv.drain(1.6)
    expect("09", "restored resize keeps chrome", drv.screen.contains(MODE_HINT),
           drv.screen.text()[-300:])
    finish(drv, world, "09")


# ── 10 shallow-tier injection timing invariant (T16) ────────────────────

def s10_injection_timing():
    """Shallow-tier timing invariant: a synthetic turn (child-report batch,
    background-task wake) is honored only when no turn is active — never
    injected into a running one (the fan-in injection bug class). One turn
    spawns a child and a fast background job, then streams ~10s while both
    complete; the mid-stream sample proves they were held. After the turn
    ends, the child batch injects as its own user turn; the background wake
    waits out its 15s quiet window and fires as its own turn. The exact
    request counts pin report-once (a duplicate wake would be a 6th)."""
    mock = MockLLM()
    mock.child_marks = ["inject-child-MARK"]
    mock.set_script([
        {"tool_call": {"name": "spawn_agent",
                       "args": {"task": "inject-child-MARK", "task_name": "inject-child"}}},
        {"tool_call": {"name": "execute_command",
                       "args": {"command": "echo bg-TAIL-MARK", "background": True,
                                "timeout_ms": 10000}}},
        {"chunks": ["s10-%d " % i for i in range(24)] + ["s10-END-MARK"], "gap": 0.4},
        # Report-length finals: a terse text-only reply trips the product's
        # short-response guard ("guard decided to continue"), which re-prompts
        # and breaks the exact request counts below.
        {"text": "batch-turn-DONE. The child report above has been summarized "
                 "for the user as requested: the sub-agent finished its task, "
                 "its findings are recapped in plain language, and there is "
                 "nothing further pending from this turn."},
        {"text": "wake-turn-DONE. The finished background task has been "
                 "reported to the user as requested: its output tail is "
                 "summarized in plain language, no further task_output call "
                 "is needed, and nothing else remains pending."},
    ])
    world, drv, ok = new_session("10", mock)
    if not expect("10", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    drv.type("run both")
    drv.key(drv.ENTER)
    s = drv.screen
    # Sample point: the stream is mid-flight (no end mark yet) and the child
    # has already returned — exactly the window the invariant protects.
    ok = drv.wait_for(
        lambda s: s.contains("s10-2") and not s.contains("s10-END-MARK")
        and len(mock.trace_of("child")) >= 1, 15.0)
    if not expect("10", "child returns while the turn is still streaming", ok,
                  drv.dump(world, "10-hold")):
        finish(drv, world, "10")
        return
    expect("10", "no synthetic turn lands mid-turn",
           not s.contains("report(s) injected into context")
           and not s.contains("waking the agent to report")
           and not s.contains("Background tasks you started have finished")
           and len(mock.trace_of("agent")) == 3,
           f"screen={s.text()[-400:]} trace={mock.trace}")
    # The turn ends; the held child batch flushes as its own user turn.
    ok = drv.wait_for(lambda s: s.contains("report(s) injected into context"), 15.0)
    ok2 = drv.wait_for(lambda s: s.contains("batch-turn-DONE"), 15.0)
    expect("10", "child batch injects after the turn ends",
           ok and ok2 and len(mock.trace_of("agent")) == 4,
           f"batch={ok} turn={ok2} trace={mock.trace}")
    # The background wake fires after its 15s quiet window — and only once.
    ok = drv.wait_for(lambda s: s.contains("waking the agent to report"), 25.0)
    ok2 = drv.wait_for(lambda s: s.contains("wake-turn-DONE"), 15.0)
    expect("10", "background wake fires after the quiet window",
           ok and ok2 and len(mock.trace_of("agent")) == 5,
           f"wake={ok} turn={ok2} trace={mock.trace}")
    finish(drv, world, "10")


# ── 11 screenshot paste pipeline (Ctrl+V) ──────────────────────────────

def tiny_png(w, h):
    """A real minimal RGB PNG — the paste path parses the header for the
    chip label, so a magic-byte stub would not do."""
    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    raw = b"".join(b"\x00" + b"\x7f\x7f\x7f" * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))


def s11_paste_image():
    """Ctrl+V takes the clipboard's image (helper seam) onto the chip row,
    the send echoes it and rides the picture to the wire as a user-message
    attachment; an empty bracketed paste does the same grab (the path a
    terminal-held Cmd+V leaves behind)."""
    import tempfile
    clip = os.path.join(tempfile.mkdtemp(prefix="smoke-clip-"), "clip.png")
    with open(clip, "wb") as f:
        f.write(tiny_png(37, 11))
    mock = MockLLM()
    mock.set_script([{"text": "screenshot noted. PASTE-DONE."}])
    world, drv, ok = new_session("11", mock, extra_env={
        "PHIMINT_CLIPBOARD_IMAGE_HELPER": f"cat {clip}"})
    if not expect("11", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    drv.key(b"\x16")  # Ctrl+V
    ok = drv.wait_for(lambda s: s.contains("📷 37x11"), 5.0)
    expect("11", "paste writes the chip row", ok, drv.dump(world, "11-chip"))
    drv.type("look at this")
    drv.key(drv.ENTER)
    ok = drv.wait_for(lambda s: s.contains("PASTE-DONE"), 12.0)
    ok2 = drv.screen.contains("📷 image 37x11")
    expect("11", "the send echoes the attachment", ok and ok2,
           f"reply={ok} echo={ok2} screen={drv.screen.text()[-400:]}")
    with mock.lock:
        carried = any("image_url" in json.dumps(r) for r in mock.requests)
    expect("11", "the picture rides the wire request", carried,
           f"requests={len(mock.requests)}")
    # An empty bracketed paste — what a terminal emits when it keeps the
    # paste key (Cmd+V) for itself and the clipboard holds a picture —
    # attaches the clipboard image all the same. The file is swapped first
    # so the new chip label is proof the second grab actually happened.
    with open(clip, "wb") as f:
        f.write(tiny_png(5, 7))
    drv.key(b"\x1b[200~\x1b[201~")
    ok = drv.wait_for(lambda s: s.contains("📷 5x7"), 5.0)
    expect("11", "an empty bracketed paste attaches the clipboard image", ok,
           drv.dump(world, "11-empty-paste"))
    finish(drv, world, "11")


# ── 12 character-precise selection (CJK) ────────────────────────────────

def s12_char_select_cjk():
    """A drag copies exactly the cells it covered, snapped to character
    edges: a press on a wide glyph's continuation cell takes the whole glyph
    ("好世界 hello" = 9 chars), a release mid-glyph ends at the glyph edge
    ("你好世" = 3 chars). On macOS the system clipboard is read back to prove
    the copy buffer really holds that text."""
    mock = MockLLM()
    mock.set_script([{"text": "你好世界 hello\nsecond-line-END"}])
    world, drv, ok = new_session("12", mock)
    if not expect("12", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return
    ok = submit_and_wait_reply(drv, "select cjk", "second-line-END", 12.0)
    if not expect("12", "reply arrives", ok, drv.dump(world, "12-wait")):
        finish(drv, world, "12")
        return
    s = drv.screen
    r = s.row_of("你好世界 hello")
    if r is None:
        expect("12", "cjk row on screen", False, s.text()[-500:])
        finish(drv, world, "12")
        return
    x0 = grid_col_of(s, r, "你好世界 hello")
    # Press on 好's continuation cell (text cell 3), release past the row
    # end: both snap outward — the copy is whole characters.
    drv.mouse(0, x0 + 3, r, press=True)
    drv.mouse(32, x0 + 25, r, press=True)
    drv.mouse(0, x0 + 25, r, press=False)
    drv.drain(0.4)
    drv.key(drv.CTRL_C)
    drv.drain(0.6)
    m = re.search(r"copied (\d+) chars", s.text())
    expect("12", "mid-glyph press snaps whole (9 chars)",
           m is not None and m.group(1) == "9", s.text()[-300:])
    if shutil.which("pbpaste"):
        got = subprocess.run(["pbpaste"], capture_output=True,
                             text=True).stdout
        expect("12", "clipboard holds the snapped slice",
               got == "好世界 hello", f"pbpaste={got!r}")
    # Second, shorter drag: press at 你, release mid-世 (text cell 5) — the
    # head snaps out to 世's edge, "你好世" = 3 chars. The changed count is
    # itself proof the second copy ran (not a stale notice).
    drv.mouse(0, x0 + 0, r, press=True)
    drv.mouse(32, x0 + 5, r, press=True)
    drv.mouse(0, x0 + 5, r, press=False)
    drv.drain(0.4)
    drv.key(drv.CTRL_C)
    drv.drain(0.6)
    m = re.search(r"copied (\d+) chars", s.text())
    expect("12", "release mid-glyph snaps to the glyph edge (3 chars)",
           m is not None and m.group(1) == "3", s.text()[-300:])
    if shutil.which("pbpaste"):
        got = subprocess.run(["pbpaste"], capture_output=True,
                             text=True).stdout
        expect("12", "clipboard holds the second slice",
               got == "你好世", f"pbpaste={got!r}")
    finish(drv, world, "12")


# ── scenario registry ───────────────────────────────────────────────────

SCENARIOS = {
    "01": ("hardware cursor + DSR", s01_hardware_cursor),
    "02": ("markdown rendering", s02_markdown),
    "03": ("completion pickers", s03_completion),
    "04": ("scrolling", s04_scroll),
    "05": ("mouse selection + copy menu", s05_selection),
    "06": ("approval gate", s06_approval),
    "07": ("task panel (multi-task)", s07_task_panel),
    "08": ("fixed protocol (Esc/Ctrl+C/Ctrl+L/OSC)", s08_fixed_protocol),
    "09": ("periodic probes (resize/liveness)", s09_periodic_probes),
    "10": ("injection timing invariant (T16)", s10_injection_timing),
    "11": ("screenshot paste pipeline (Ctrl+V / empty paste)", s11_paste_image),
    "12": ("character-precise selection (CJK snap + clipboard)", s12_char_select_cjk),
    "13": ("model route: spawn vs /model hot-swap", s13_spawn_vs_model_hotswap),
    "14": ("model route: judge stays on lite", s14_judge_stays_lite),
    "15": ("model route: wire invariant + audit", s15_wire_invariant),
}


def main():
    # Contract preflight (T7/SC-4): pin the spawn↔mark wire contract before
    # any scenario runs — pure Python, needs no binary. The summary mark is
    # pinned with it (cross-repo constant against agent-works).
    child_marks_contract_selftest()
    summary_mark_contract_selftest()
    if not os.path.isfile(BIN):
        print(f"build phimint first: {BIN}")
        return 2
    ids = sys.argv[1:] or sorted(SCENARIOS)
    for sid in ids:
        name, fn = SCENARIOS[sid]
        print(f"\n─── {sid} {name} ───")
        try:
            fn()
        except Exception as e:  # a crashed scenario is a failed scenario
            record(sid, name, False, f"exception: {e!r}")
    return summary()


if __name__ == "__main__":
    sys.exit(main())
