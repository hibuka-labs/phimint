#!/usr/bin/env python3
"""Bare-PTY harness for the shell smoke suite (design Issue 3.2).

The craft formalized here, once hand-rolled per session:
  - DSR auto-response (the TUI asks `ESC [ 6n` at boot; a silent PTY wedges it)
  - TIOCSWINSZ (rows/cols must be set before the first draw)
  - HOME isolation (fixture HOME + workspace; never touch the real ~/.phimint)
  - CSI replay into a cell grid — byte streams lie about what is on screen,
    so every absence claim ("the popup is gone") asserts against the grid,
    with East-Asian-width advance so CJK lines land at the right columns

Everything here is product-agnostic except `write_config()` and `spawn_argv()`,
which encode one product's CLI/config shape. A second consumer lifts the
harness and replaces those two functions.
"""

import codecs
import fcntl
import json
import os
import re
import select
import shutil
import signal
import struct
import sys
import termios
import threading
import time
import unicodedata
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# ── accounting ──────────────────────────────────────────────────────────

RESULTS = []


def record(tid, name, ok, detail=""):
    RESULTS.append((tid, name, ok, detail))
    print(f"[{'PASS' if ok else 'FAIL'}] {tid} {name}" + (f" — {detail}" if detail else ""))


def expect(tid, name, ok, detail=""):
    record(tid, name, bool(ok), "" if ok else detail)
    return bool(ok)


def summary():
    failed = [r for r in RESULTS if not r[2]]
    print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} passed")
    for tid, name, _, detail in failed:
        print(f"  FAIL {tid} {name}" + (f" — {detail}" if detail else ""))
    return 1 if failed else 0


# ── Screen: byte stream → grid (字节流≠屏幕) ────────────────────────────


def ch_width(ch: str) -> int:
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


class Screen:
    """Replay a terminal byte stream into a rows×cols cell grid.

    Handles the CSI subset ratatui/crossterm emit: CUP/CUU/CUD/CUF/CUB/CHA/
    VPA, ED/EL/ECH, SGR (ignored), private mode sets (alt-screen ?1049).
    Tracks the pen so tests can assert *hardware cursor* placement: after a
    drain the pen is where the TUI last pointed the terminal cursor.
    """

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.clear()
        self.dec = codecs.getincrementaldecoder("utf-8")(errors="replace")
        self._buf = ""
        self._i = 0

    def clear(self):
        self.grid = [[" "] * self.cols for _ in range(self.rows)]
        self.cy = self.cx = 0

    def feed(self, data: bytes):
        self._buf += self.dec.decode(data)
        self._run()

    def _run(self):
        s, i = self._buf, self._i
        n = len(s)
        while i < n:
            ch = s[i]
            if ch == "\x1b":
                if i + 1 >= n:
                    break  # incomplete escape; wait for more
                nxt = s[i + 1]
                if nxt == "[":
                    j = i + 2
                    while j < n and not ("\x40" <= s[j] <= "\x7e"):
                        j += 1
                    if j >= n:
                        break
                    self._csi(s[i + 2 : j], s[j])
                    i = j + 1
                    continue
                if nxt == "]":
                    j = i + 2
                    while j < n:
                        if s[j] == "\x07":
                            break
                        if s[j] == "\x1b" and j + 1 < n and s[j + 1] == "\\":
                            j += 1
                            break
                        j += 1
                    i = j + 1
                    continue
                i += 2  # two-char escape: ignore
                continue
            if ch == "\r":
                self.cx = 0
            elif ch == "\n":
                self.cy += 1
                if self.cy >= self.rows:
                    self._scroll()
                    self.cy = self.rows - 1
            elif ch == "\x08":
                self.cx = max(0, self.cx - 1)
            elif ch == "\t":
                self.cx = min(self.cols - 1, (self.cx // 8 + 1) * 8)
            elif ch >= " ":
                self._put(ch)
            i += 1
        self._buf = s[i:]
        self._i = 0

    def _put(self, ch):
        w = ch_width(ch)
        if w == 0:
            return
        if self.cx + w > self.cols:
            self.cx = 0
            self.cy += 1
            if self.cy >= self.rows:
                self._scroll()
                self.cy = self.rows - 1
        self._clear_span(self.cy, self.cx, w)
        self.grid[self.cy][self.cx] = ch
        for k in range(1, w):
            self.grid[self.cy][self.cx + k] = ""
        self.cx += w

    def _clear_span(self, r, c, w):
        for k in range(w):
            if 0 <= c + k < self.cols:
                self.grid[r][c + k] = " "

    def _scroll(self):
        self.grid.pop(0)
        self.grid.append([" "] * self.cols)

    def _csi(self, params: str, final: str):
        priv = params.startswith("?")
        raw = params[1:] if priv else params
        nums = [int(p) if p.isdigit() else 0 for p in raw.split(";")] if raw else [0]
        while len(nums) < 2:
            nums.append(0)

        if final in ("H", "f"):
            self.cy = min(self.rows - 1, max(0, (nums[0] or 1) - 1))
            self.cx = min(self.cols - 1, max(0, (nums[1] or 1) - 1))
        elif final == "A":
            self.cy = max(0, self.cy - max(1, nums[0]))
        elif final == "B":
            self.cy = min(self.rows - 1, self.cy + max(1, nums[0]))
        elif final == "C":
            self.cx = min(self.cols - 1, self.cx + max(1, nums[0]))
        elif final == "D":
            self.cx = max(0, self.cx - max(1, nums[0]))
        elif final == "G":
            self.cx = min(self.cols - 1, max(0, (nums[0] or 1) - 1))
        elif final == "d":
            self.cy = min(self.rows - 1, max(0, (nums[0] or 1) - 1))
        elif final == "J":
            mode = nums[0]
            if mode in (2, 3):
                self.clear()
            elif mode == 0:
                for c in range(self.cx, self.cols):
                    self.grid[self.cy][c] = " "
                for r in range(self.cy + 1, self.rows):
                    self.grid[r] = [" "] * self.cols
            elif mode == 1:
                for r in range(0, self.cy):
                    self.grid[r] = [" "] * self.cols
                for c in range(0, self.cx + 1):
                    self.grid[self.cy][c] = " "
        elif final == "K":
            mode = nums[0]
            if mode == 0:
                for c in range(self.cx, self.cols):
                    self.grid[self.cy][c] = " "
            elif mode == 1:
                for c in range(0, self.cx + 1):
                    self.grid[self.cy][c] = " "
            else:
                self.grid[self.cy] = [" "] * self.cols
        elif final == "X":
            self._clear_span(self.cy, self.cx, max(1, nums[0]))
        elif final in ("h", "l") and priv:
            if "1049" in raw and final == "h":
                self.clear()  # alt-screen: new blank canvas

    def lines(self):
        return ["".join(row).rstrip() for row in self.grid]

    def text(self):
        return "\n".join(self.lines())

    def contains(self, needle: str) -> bool:
        return any(needle in ln for ln in self.lines())

    def row_of(self, needle: str):
        for r, ln in enumerate(self.lines()):
            if needle in ln:
                return r
        return None

    def cursor(self):
        return self.cy, self.cx

    def dump(self, path):
        with open(path, "w") as fh:
            fh.write(self.text() + "\n")


# ── Mock LLM: scripted OpenAI SSE ───────────────────────────────────────

# Hyphen-agnostic anchor: keys on the phrase "completion judge" (present in
# the judge prompt's first line regardless of hyphenation) — a full-sentence
# mark silently broke once when the prompt was reworded (20261107_27dc727a).
TITLE_MARK = "You are a title generator"
JUDGE_MARK = "completion judge"

# The watcher's focus-summary prompt for a finished child: its FIRST user
# message is the composed report block, not the task text. Distinct from the
# child's own run (whose first user message is the task) — without this class
# the summary call masquerades as a second child request on the wire.
#
# Cross-repo constant: agent-works composes the report block as
# `【key】\nvalue` (focus/core.rs `Context::to_prompt`) and its summarizer
# names the first field `sub-agent` (focus/progress.rs
# `Context::add("sub-agent", ...)`). A rename there silently reclassifies
# announcer calls — `summary_mark_contract_selftest` pins this.
SUMMARY_MARK = "【sub-agent】"

# The spawn tool's wire name (phi-kernel-tools `spawn_agent`). A scripted
# spawn's `task` text is the only child-identification channel MockLLM has —
# see `check_child_marks`.
SPAWN_TOOL_NAMES = {"spawn_agent"}


def check_child_marks(entries, child_marks):
    """Precheck the spawn↔mark wire contract (SC-4).

    MockLLM identifies a spawned child by scenario-registered `child_marks`
    matched against the child's FIRST user message — which carries the task
    text the scenario passed to the spawn tool. A scripted spawn whose task
    carries no registered mark would silently classify the child's
    conversation as a parent continuation (the child then eats the parent
    script — the 20261107_27dc727a failure class). Raise at set-up instead.
    """
    for entry in entries:
        tc = entry.get("tool_call") or {}
        if tc.get("name") not in SPAWN_TOOL_NAMES:
            continue
        raw_args = tc.get("args")
        args = raw_args if isinstance(raw_args, dict) else {}
        task = args.get("task") or ""
        if not any(mark in task for mark in child_marks):
            raise ValueError(
                f"child_marks contract: spawn task {task!r} carries no "
                f"registered mark in {child_marks!r} — the spawned child "
                "would be misclassified as a parent continuation"
            )


def child_marks_contract_selftest():
    """Pin the spawn↔mark contract (T7/SC-4) — pure Python, runs pre-scenario.

    Guards both halves: `check_child_marks` itself (a rewording of the
    classification rules must not silently weaken it) and the wiring into
    `MockLLM.set_script` (the precheck must actually run at set-up).
    """
    good = [{"tool_call": {"name": "spawn_agent",
                           "args": {"task": "run the MARK task"}}}]
    cases = [
        ("marked spawn passes", ["MARK"], good, True),
        ("unmarked spawn is rejected", ["MARK"],
         [{"tool_call": {"name": "spawn_agent", "args": {"task": "no mark"}}}],
         False),
        ("spawn without a task is rejected", ["MARK"],
         [{"tool_call": {"name": "spawn_agent", "args": {"task_name": "t"}}}],
         False),
        ("non-spawn tools are exempt", [],
         [{"tool_call": {"name": "read_file", "args": {"task": "no mark"}}}],
         True),
        ("spawn with no marks registered is rejected", [], good, False),
    ]
    ok_all = True
    for name, marks, entries, should_pass in cases:
        try:
            check_child_marks(entries, marks)
            ok = should_pass
        except ValueError:
            ok = not should_pass
        ok_all &= expect("00", name, ok)
    # The wiring pin: set_script must enforce the same contract.
    mock = MockLLM()
    mock.child_marks = ["MARK"]
    try:
        mock.set_script([{"tool_call": {"name": "spawn_agent",
                                        "args": {"task": "no mark"}}}])
        wired = False
    except ValueError:
        wired = True
    mock.server.shutdown()
    ok_all &= expect("00", "MockLLM.set_script enforces the contract", wired)
    return ok_all


def summary_mark_contract_selftest():
    """Pin SUMMARY_MARK against the family composition contract.

    The mark must be a `【key】` focus field (the shape `Context::to_prompt`
    composes), and the key must still exist in agent-works' summarizer when
    its source checkout is reachable (dev machines) — a rename there would
    silently reclassify announcer calls. Without the sibling checkout (CI)
    only the shape is checked.
    """
    ok = True
    m = re.fullmatch(r"【([^【】]+)】", SUMMARY_MARK)
    ok &= expect("00", "SUMMARY_MARK is one `【key】` focus field", m is not None)
    key = m.group(1) if m else ""
    progress_src = (
        Path(__file__).resolve().parents[2].parent
        / "agent-works" / "src" / "focus" / "progress.rs"
    )
    if progress_src.is_file():
        body = progress_src.read_text(encoding="utf-8")
        ok &= expect(
            "00",
            f"agent-works still names the {key!r} focus field",
            f'add("{key}"' in body,
        )
    else:
        ok &= expect(
            "00",
            "SUMMARY_MARK shape pinned (no agent-works sibling for the key check)",
            True,
        )
    return ok


def _content_text(content):
    """Text of a message's `content`: a plain string on text-only turns, but
    a part list on multimodal ones (the screenshot paste path sends
    `{"type": "image_url", ...}` parts) — keep the text parts only."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return " ".join(
            p.get("text", "") for p in content
            if isinstance(p, dict) and p.get("type") == "text"
        )
    return ""


def classify_request(req, child_marks):
    """Route class of one wire request: 'title' | 'judge' | 'summary' | 'child'
    | 'agent'.

    The single predicate source for both `MockLLM.handle`'s dispatch and the
    scenarios' wire assertions — the two can never drift apart. Branch order
    matters: the title generator's prompt blob matches first, then the judge's
    system-message mark, then the watcher's focus-summary (its FIRST user
    message is the composed report block), then a spawned child (its FIRST
    user message carries the task mark — checking the whole request would
    misclassify a parent continuation quoting the task text).
    """
    text_blob = json.dumps(req.get("messages", []), ensure_ascii=False)
    if TITLE_MARK in text_blob:
        return "title"
    sys_msg = " ".join(
        _content_text(m.get("content", ""))
        for m in req.get("messages", [])
        if m.get("role") == "system"
    )
    if JUDGE_MARK in sys_msg:
        return "judge"
    first_user = next(
        (_content_text(m.get("content", "")) for m in req.get("messages", [])
         if m.get("role") == "user"),
        "",
    )
    if first_user.startswith(SUMMARY_MARK):
        return "summary"
    return "child" if any(mark in first_user for mark in child_marks) else "agent"


class MockLLM:
    """Scripted OpenAI-compatible SSE server.

    Requests classified first (title generator / completion judge get canned
    answers so they never eat the script; requests carrying any of
    `child_marks` — the task text a scenario passes to a spawn tool — get
    `child_reply` so a spawned child always finishes); everything else pops
    the next script entry:
        {"tool_call": {"name": ..., "args": {...}}}
        {"text": "...", "delay": 0.0}      # delay before the first chunk
        {"chunks": ["a", "b"], "gap": 0.2} # paced streaming
    """

    def __init__(self):
        self.script = []
        self.lock = threading.Lock()
        self.trace = []
        # Full request bodies, in arrival order — scenarios can assert on
        # wire facts (e.g. "this request carried an image").
        self.requests = []
        self.child_marks = []
        self.child_reply = "child report: ok"
        self.default_reply = "script exhausted"
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self._handler())
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def _handler(self):
        outer = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                try:
                    req = json.loads(body or b"{}")
                except Exception:
                    req = {}
                outer.handle(self, req)

        return Handler

    def set_script(self, entries):
        # Fail fast on a spawn whose task carries no registered mark (SC-4).
        check_child_marks(entries, self.child_marks)
        with self.lock:
            self.script = list(entries)

    def handle(self, hs, req):
        with self.lock:
            self.requests.append(req)
        kind = classify_request(req, self.child_marks)
        if kind == "title":
            with self.lock:
                self.trace.append("title")
            self._json(hs, {
                "choices": [{"message": {"content": "pty smoke session"},
                             "finish_reason": "stop"}]
            })
            return
        if kind == "judge":
            with self.lock:
                self.trace.append("judge")
            self._json(hs, {
                "choices": [{"message": {"content": '{"done": true, "reason": "scripted"}'},
                             "finish_reason": "stop"}]
            })
            return

        # A spawned child's conversation is not the parent's script: answer
        # it deterministically so `spawn_agent` scenarios always complete.
        # The watcher's focus-summary call rides the same canned answer (the
        # mock's reply is not summary JSON, so the product falls back to its
        # plain status word — today's shipped behavior). Classification
        # rules live in `classify_request`.
        with self.lock:
            if kind in ("child", "summary"):
                self.trace.append(kind)
                entry = {"text": self.child_reply}
            else:
                entry = self.script.pop(0) if self.script else {"text": self.default_reply}
                self.trace.append("agent:" + json.dumps(entry, ensure_ascii=False)[:60])

        # Defense in depth: entries mutated past `set_script` bypass the
        # set-up precheck — re-verify at use, loudly, instead of letting an
        # unmarked spawn silently eat the parent script.
        check_child_marks([entry], self.child_marks)

        delay = float(entry.get("delay", 0) or 0)
        if delay:
            time.sleep(delay)
        if "tool_call" in entry:
            tc = entry["tool_call"]
            args = json.dumps(tc["args"], ensure_ascii=False)
            delta = {"tool_calls": [{"index": 0, "id": "call_1",
                                     "function": {"name": tc["name"], "arguments": args}}]}
            finish = "tool_calls"
            chunks, gap = None, 0
        elif "chunks" in entry:
            chunks, gap, delta, finish = entry["chunks"], float(entry.get("gap", 0.2)), None, "stop"
        else:
            delta = {"content": entry.get("text", "")}
            finish = "stop"
            chunks, gap = None, 0

        if req.get("stream"):
            if chunks is not None:
                self._sse_paced(hs, chunks, gap)
            else:
                self._sse(hs, delta, finish)
        else:
            content = entry.get("text", "") if "text" in entry else ""
            self._json(hs, {
                "choices": [{"message": {"content": content}, "finish_reason": finish}]
            })

    def _sse(self, hs, delta, finish_reason):
        hs.send_response(200)
        hs.send_header("Content-Type", "text/event-stream")
        hs.send_header("Cache-Control", "no-cache")
        hs.send_header("Connection", "close")
        hs.end_headers()
        hs.close_connection = True

        def chunk(payload):
            hs.wfile.write(b"data: " + json.dumps(payload).encode() + b"\n\n")

        chunk({"id": "mock", "object": "chat.completion.chunk", "model": "mock",
               "choices": [{"index": 0, "delta": delta, "finish_reason": None}]})
        chunk({"id": "mock", "object": "chat.completion.chunk", "model": "mock",
               "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]})
        hs.wfile.write(b"data: [DONE]\n\n")
        try:
            hs.wfile.flush()
        except Exception:
            pass

    def _sse_paced(self, hs, chunks, gap):
        hs.send_response(200)
        hs.send_header("Content-Type", "text/event-stream")
        hs.send_header("Cache-Control", "no-cache")
        hs.send_header("Connection", "close")
        hs.end_headers()
        hs.close_connection = True

        def chunk(payload):
            hs.wfile.write(b"data: " + json.dumps(payload).encode() + b"\n\n")
            try:
                hs.wfile.flush()
            except Exception:
                pass

        for piece in chunks:
            chunk({"id": "mock", "object": "chat.completion.chunk", "model": "mock",
                   "choices": [{"index": 0, "delta": {"content": piece},
                                "finish_reason": None}]})
            time.sleep(gap)
        chunk({"id": "mock", "object": "chat.completion.chunk", "model": "mock",
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        hs.wfile.write(b"data: [DONE]\n\n")
        try:
            hs.wfile.flush()
        except Exception:
            pass

    def _json(self, hs, payload):
        raw = json.dumps(payload).encode()
        hs.send_response(200)
        hs.send_header("Content-Type", "application/json")
        hs.send_header("Content-Length", str(len(raw)))
        hs.end_headers()
        hs.wfile.write(raw)

    def trace_of(self, kind):
        with self.lock:
            return [t for t in self.trace if t.startswith(kind)]

    def requests_of(self, kind):
        """Every captured request of one route class (see `classify_request`)."""
        with self.lock:
            return [r for r in self.requests
                    if classify_request(r, self.child_marks) == kind]


# ── spawn: isolated HOME + workspace + fixture config ───────────────────

ROWS, COLS = 40, 120


def write_config(home, mock_port, tiers=None):
    """One product's fixture config — the part a second consumer replaces.

    `tiers` overrides any of main/lite/advanced verbatim: a plain string is a
    same-endpoint Simple tier, a dict a Full tier (own model/base_url/api_key/
    protocol). Routing scenarios need this to make each tier's wire model
    attributable and to build one cross-endpoint tier.
    """
    cfg = {
        "base_url": f"http://127.0.0.1:{mock_port}",
        "api_key": "mock-key",
        "protocol": "openai",
        "main": "mock-model",
        "lite": "mock-model",
        "advanced": "mock-model",
        "ui": {"tips": {"enabled": False}},
    }
    for tier, value in (tiers or {}).items():
        cfg[tier] = value
    cfg_path = os.path.join(home, ".phimint", "config.json")
    os.makedirs(os.path.dirname(cfg_path), exist_ok=True)
    with open(cfg_path, "w") as fh:
        json.dump(cfg, fh)
    return cfg_path


def spawn_argv(binary, ws, cfg_path, session_id, approval=None):
    """One product's CLI shape — the part a second consumer replaces."""
    argv = [
        binary,
        "--workspace", ws,
        "--config", cfg_path,
        "--model", "mock-model",
        "--base-url", "",  # filled from config; kept for products that require it
        "--api-key", "mock-key",
        "--protocol", "openai",
        "--no-update-check",
        "--banner", "off",
        "--reasoning-effort", "none",
        "--session", session_id,
    ]
    if approval:
        argv += ["--approval", approval]
    return argv


class World:
    """One scenario's isolated HOME/workspace/out directories."""

    def __init__(self, tag):
        self.home = f"/tmp/pty-smoke-{tag}-home"
        self.ws = f"/tmp/pty-smoke-{tag}-ws"
        self.out = f"/tmp/pty-smoke-{tag}-out"
        for d in (self.home, self.ws, self.out):
            shutil.rmtree(d, ignore_errors=True)
            os.makedirs(d)

    def sessions_dir(self):
        return os.path.join(self.home, ".phimint", "sessions")


def spawn(binary, world, mock, session_id, approval=None, extra_env=None,
          extra_args=None, tiers=None):
    """Fork the TUI on a fresh PTY. Returns (pid, master_fd, slave_fd) — the
    slave fd is kept so tests can *paint* the terminal behind the TUI's back
    (a write to the slave is terminal output, exactly like a misbehaving
    child's stderr — the corruption class Ctrl+L must heal)."""
    cfg_path = write_config(world.home, mock.port, tiers)
    # The product reads base_url from the config; the CLI mirror points at /v1.
    argv = spawn_argv(binary, world.ws, cfg_path, session_id, approval)
    for i, a in enumerate(argv):
        if a == "--base-url":
            argv[i + 1] = f"http://127.0.0.1:{mock.port}/v1"
    if extra_args:
        argv += list(extra_args)
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    pid = os.fork()
    if pid == 0:
        os.setsid()
        try:
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
        except OSError:
            pass
        os.dup2(slave, 0)
        os.dup2(slave, 1)
        os.dup2(slave, 2)
        if slave > 2:
            os.close(slave)
        os.close(master)
        env = os.environ.copy()
        env["HOME"] = world.home
        env["TERM"] = "xterm-256color"
        for k in ("LLM_CONFIG", "NOTION_TOKEN", "PHIMINT_CANVAS_CRASH_AFTER_MS"):
            env.pop(k, None)
        if extra_env:
            env.update(extra_env)
        os.chdir(world.ws)
        os.execve(binary, argv, env)
        os._exit(127)
    return pid, master, slave


class Driver:
    """Drive one spawned TUI: drain (with DSR answers), keys, mouse, resize,
    and slave-side paint (terminal output written behind the TUI's back)."""

    ESC = b"\x1b"
    ENTER = b"\r"
    TAB = b"\t"
    SHIFT_TAB = b"\x1b[Z"
    PG_UP = b"\x1b[5~"
    PG_DOWN = b"\x1b[6~"
    CTRL_C = b"\x03"
    CTRL_L = b"\x0c"
    CTRL_D = b"\x04"

    def __init__(self, pid, fd, slave=None, rows=ROWS, cols=COLS):
        self.pid, self.fd, self.slave = pid, fd, slave
        self.rows, self.cols = rows, cols
        self.screen = Screen(rows, cols)
        self.dsr_answered = 0

    def paint(self, data: bytes):
        """Write terminal *output* to the slave: what a foreign process
        sharing the tty would paint. Not keyboard input."""
        if self.slave is not None:
            os.write(self.slave, data)

    def drain(self, seconds):
        end = time.time() + seconds
        acc = b""
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            acc += chunk
            if b"\x1b[6n" in chunk:  # DSR cursor report — answer or the TUI wedges
                self._write(f"\x1b[{self.rows};1R".encode())
                self.dsr_answered += 1
        self.screen.feed(acc)
        return acc

    def wait_for(self, pred, timeout=8.0, label="condition"):
        """Drain until pred(screen) or timeout; returns (ok, last screen)."""
        end = time.time() + timeout
        while time.time() < end:
            if pred(self.screen):
                return True
            self.drain(0.15)
        return pred(self.screen)

    def type(self, text):
        self._write(text.encode())

    def key(self, b):
        self._write(b)

    def _write(self, data):
        # Once the child exits its PTY slave closes and every write (and most
        # reads) raise EIO — that is "the TUI is gone", never a test failure.
        try:
            os.write(self.fd, data)
        except OSError:
            pass

    def mouse(self, button, col, row, press=True):
        """SGR mouse at 0-based grid (col, row) — converted to the wire's
        1-based form here. button 0=left 2=right 32=left-drag 64/65=wheel."""
        final = b"M" if press else b"m"
        self._write(f"\x1b[<{button};{col + 1};{row + 1}".encode() + final)

    def wheel(self, up, col=10, row=20):
        self.mouse(64 if up else 65, col, row)

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.screen.rows, self.screen.cols = rows, cols
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def quit(self):
        self.key(self.CTRL_C)
        self.drain(0.5)
        self.key(self.CTRL_C)
        self.drain(0.5)
        self.key(self.CTRL_D)
        for _ in range(25):
            wpid, _ = os.waitpid(self.pid, os.WNOHANG)
            if wpid == self.pid:
                self.drain(1.2)  # exit replay
                return True
            self.drain(0.3)
        os.kill(self.pid, signal.SIGKILL)
        os.waitpid(self.pid, 0)
        return False

    def alive(self):
        wpid, _ = os.waitpid(self.pid, os.WNOHANG)
        return wpid == 0

    def dump(self, world, tag):
        path = os.path.join(world.out, f"screen-{int(time.time() * 1000)}-{tag}.txt")
        self.screen.dump(path)
        return path


def boot_ok(drv, timeout=12.0):
    """Wait for the first full draw (status bar present) — proves DSR was
    answered and the event loop is alive."""
    return drv.wait_for(lambda s: s.contains("mock-model") or s.contains("/"), timeout)
