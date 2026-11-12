#!/usr/bin/env python3
"""Model-route E2E (T8) — the two routing seams under a real boot.

The unit tests in `src/agent/spawn_resolve.rs` / `src/model/lite_pin.rs`
prove the resolution rules in isolation; only a booted TUI on a mock wire
proves the product path end to end (the mounted factory, the pinned judge,
and the bodies that actually leave the process):

  13 — spawn vs `/model` hot-swap: resolution is live per spawn (`/lite`
       retargets the very next child) and the hot-swapped main never
       retargets a spawn (the sub_agent scene routes to the lite tier).
  14 — judge vs `/model`: the completion judge stays on the lite tier after
       a `/model` pick (decision 3 — /model reaches the judge only
       indirectly, through an unconfigured lite falling to main).
  15 — wire invariant: a tier token never reaches `body["model"]` — plus
       the cross-endpoint fallback (no override + a persistent notice) and
       the per-spawn audit account.

Turn anatomy (the script budget — a spawned turn pops THREE entries):
  1. the parent's call that fires `spawn_agent`
  2. the parent's continuation after the tool result (the DONE text)
  3. the child-result injection's synthetic turn (the ACK text) — the
     shell holds the batch while the parent runs and flushes it as a
     synthetic turn right after
Plus two canned, non-popping calls per child: the child's own run (carries
the resolved model — the routing claim) and the watcher's focus-summary
call (runs on the boot client, un-routed: the scene table's `tool_summary`
placeholder actually has a consumer — see the notes).

Each scenario needs attributable per-tier names, so it passes `tiers=` into
the harness fixture config; `main` stays "mock-model" (the boot name the
CLI also passes) so the Inherit fallback is distinguishable on the wire.

Usage (via smoke.py):  python3 scripts/pty_smoke/smoke.py 13 14 15
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (  # noqa: E402
    Driver, MockLLM, World, boot_ok, classify_request, expect, spawn,
)

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
# Absolute, for the same reason smoke.py resolves it absolutely: `spawn`
# chdirs into the isolated workspace before execve.
BIN = os.path.abspath(
    os.environ.get("PHIMINT_BIN", os.path.join(ROOT, "target", "debug", "phimint"))
)

# Tier tokens: the vocabulary the resolver must kill before the wire.
TIER_TOKENS = {"main", "lite", "advanced"}


# ── helpers ─────────────────────────────────────────────────────────────


def boot(tag, mock, tiers):
    world = World(tag)
    pid, fd, slave = spawn(BIN, world, mock, f"smoke-{tag}", tiers=tiers)
    drv = Driver(pid, fd, slave)
    return world, drv, boot_ok(drv)


def finish(drv, world, tag):
    drv.dump(world, tag)
    drv.quit()


def run_command(drv, cmd, mark, timeout=8.0):
    """Type a slash command and wait for its command reply on screen.

    A command with arguments never matches a completion row (the picker's
    prefix match can't span the space), so Enter submits it verbatim — the
    same policy line the composer enforces.
    """
    drv.type(cmd)
    drv.key(drv.ENTER)
    return drv.wait_for(lambda s: s.contains(mark), timeout)


def spawn_entry(mark, model=None, task_name="t"):
    """One scripted `spawn_agent` call; the task carries the child's mark
    (the spawn↔mark wire contract the harness prechecks)."""
    args = {"task_name": task_name, "task": f"run the {mark} job"}
    if model is not None:
        args["model"] = model
    return {"tool_call": {"name": "spawn_agent", "args": args}}


def spawn_turn(mark, done_text, ack_text, model=None, task_name="t"):
    """One spawned turn's full LLM budget — three pops, in order (see the
    module docstring's turn anatomy)."""
    return [
        spawn_entry(mark, model=model, task_name=task_name),
        {"text": done_text},
        {"text": ack_text},
    ]


def run_spawn_turn(drv, prompt, done_mark, ack_mark, timeout=25.0):
    """Drive one spawned turn to its injection ACK; return (turn completed,
    the DONE reply still visible on screen)."""
    drv.type(prompt)
    drv.key(drv.ENTER)
    ok = drv.wait_for(lambda s: s.contains(ack_mark), timeout)
    return ok, drv.screen.contains(done_mark)


def child_models(mock, mark):
    """`body["model"]` of every request belonging to the child marked `mark`
    (the child's own run — the watcher's focus-summary call is a separate
    class and does not appear here), in arrival order."""
    out = []
    with mock.lock:
        for req in mock.requests:
            if classify_request(req, mock.child_marks) != "child":
                continue
            first_user = next(
                (m.get("content", "") or ""
                 for m in req.get("messages", []) if m.get("role") == "user"),
                "",
            )
            if mark in first_user:
                out.append(req.get("model"))
    return out


def child_ran_on(mock, mark, want):
    """The child's requests all carried `want` (at least one request —
    exact counts are the child loop's business, not the routing claim)."""
    got = child_models(mock, mark)
    return bool(got) and all(m == want for m in got), got


def screen_has(scr, needle):
    """Grid search that survives word wrap: rows are also joined at wrap
    boundaries (a wrapped mark never lands whole on one row)."""
    if scr.contains(needle):
        return True
    joined = " ".join(scr.lines())
    return needle in joined or needle in "".join(scr.lines())


def audit_lines(world):
    """The model-route audit account (one JSON line per spawn)."""
    d = os.path.join(world.home, ".phimint", "model-route")
    lines = []
    for name in sorted(os.listdir(d)) if os.path.isdir(d) else []:
        with open(os.path.join(d, name)) as fh:
            for raw in fh:
                raw = raw.strip()
                if raw:
                    lines.append(json.loads(raw))
    return lines


# ── 13 spawn vs /model hot-swap ─────────────────────────────────────────


def s13_spawn_vs_model_hotswap():
    """Per-spawn live resolution, and the hot-swap boundary.

    An omitted `model` resolves through the sub_agent scene to the lite
    tier's REAL name; `/lite` retargets the very next spawn (nothing is
    frozen at build); and after `/model` moves main, spawns still route to
    lite — a hot-swap of the main chat never retargets a child default.
    """
    mock = MockLLM()
    mock.child_marks = ["MARK13A", "MARK13B", "MARK13C"]
    mock.set_script(
        spawn_turn("MARK13A", "SPAWN-A-DONE", "INJECT-A-ACK", task_name="a")
        + spawn_turn("MARK13B", "SPAWN-B-DONE", "INJECT-B-ACK", task_name="b")
        + spawn_turn("MARK13C", "SPAWN-C-DONE", "INJECT-C-ACK", task_name="c")
    )
    world, drv, ok = boot("13", mock, {"lite": "lite-alpha", "advanced": "adv-alpha"})
    if not expect("13", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return

    # 1) omitted model → the sub_agent scene's tier, real name on the wire.
    done, visible = run_spawn_turn(drv, "go a", "SPAWN-A-DONE", "INJECT-A-ACK")
    on, got = child_ran_on(mock, "MARK13A", "lite-alpha")
    expect("13", "spawn a completes", done and visible, drv.dump(world, "13-a"))
    expect("13", "omitted model resolves to the live lite name",
           on, f"child bodies: {got}")

    # 2) /lite retargets the very next spawn — resolution is per spawn.
    if not expect("13", "/lite command runs",
                  run_command(drv, "/lite lite-beta", "Lite model set to: lite-beta"),
                  drv.dump(world, "13-lite-cmd")):
        finish(drv, world, "13")
        return
    done, visible = run_spawn_turn(drv, "go b", "SPAWN-B-DONE", "INJECT-B-ACK")
    on, got = child_ran_on(mock, "MARK13B", "lite-beta")
    expect("13", "spawn b completes", done and visible, drv.dump(world, "13-b"))
    expect("13", "the next spawn picks up the new lite name",
           on, f"child bodies: {got}")

    # 3) /model hot-swaps main; the spawn default does NOT follow it.
    if not expect("13", "/model command runs",
                  run_command(drv, "/model picked-alpha",
                              "Main model set to: picked-alpha"),
                  drv.dump(world, "13-model-cmd")):
        finish(drv, world, "13")
        return
    done, visible = run_spawn_turn(drv, "go c", "SPAWN-C-DONE", "INJECT-C-ACK")
    on, got = child_ran_on(mock, "MARK13C", "lite-beta")
    expect("13", "spawn c completes", done and visible, drv.dump(world, "13-c"))
    expect("13", "a /model hot-swap never retargets the spawn default",
           on, f"child bodies: {got}")
    # Positive control: the hot-swap really landed on the main chat —
    # without this the row above would pass vacuously on a dead /model.
    agent_models = [r.get("model") for r in mock.requests_of("agent")]
    expect("13", "the hot-swap landed on the main chat",
           bool(agent_models) and agent_models[-1] == "picked-alpha",
           f"agent bodies: {agent_models}")
    finish(drv, world, "13")


# ── 14 judge vs /model ──────────────────────────────────────────────────


def s14_judge_stays_lite():
    """Decision 3 on the wire: the completion judge runs on the lite tier
    and a `/model` pick does not retarget it — while the main chat follows
    the pick (same positive control as 13).

    The judge only judges tool-using runs with a short final reply, so each
    judged turn scripts [a real tool call, a short text] — the recipe the
    notice E2E uses. No spawn → no injection turn → two pops per turn.
    """
    mock = MockLLM()
    mock.set_script([
        {"tool_call": {"name": "list_files", "args": {"path": "."}}},
        {"text": "ok-done"},
        {"tool_call": {"name": "list_files", "args": {"path": "."}}},
        {"text": "ok-again"},
    ])
    world, drv, ok = boot("14", mock, {"lite": "lite-alpha", "advanced": "adv-alpha"})
    if not expect("14", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return

    # 1) a judged turn before any pick: the judge runs on lite.
    drv.type("check files")
    drv.key(drv.ENTER)
    ok = drv.wait_for(lambda s: s.contains("ok-done"), 15.0)
    ok &= _wait_for_judges(drv, mock, 1)
    judges = [r.get("model") for r in mock.requests_of("judge")]
    expect("14", "first judged turn completes", ok, drv.dump(world, "14-a"))
    expect("14", "the judge runs on the lite tier",
           len(judges) >= 1 and judges[0] == "lite-alpha", f"judge bodies: {judges}")

    # 2) /model moves main; the judge must stay put.
    if not expect("14", "/model command runs",
                  run_command(drv, "/model picked-alpha",
                              "Main model set to: picked-alpha"),
                  drv.dump(world, "14-model-cmd")):
        finish(drv, world, "14")
        return
    drv.type("check again")
    drv.key(drv.ENTER)
    ok = drv.wait_for(lambda s: s.contains("ok-again"), 15.0)
    ok &= _wait_for_judges(drv, mock, 2)
    judges = [r.get("model") for r in mock.requests_of("judge")]
    expect("14", "second judged turn completes", ok, drv.dump(world, "14-b"))
    expect("14", "a /model pick never reaches the judge",
           len(judges) >= 2 and all(m == "lite-alpha" for m in judges),
           f"judge bodies: {judges}")
    # Positive control: the pick DID move the main chat.
    agent_models = [r.get("model") for r in mock.requests_of("agent")]
    expect("14", "the pick landed on the main chat",
           "picked-alpha" in agent_models, f"agent bodies: {agent_models}")
    finish(drv, world, "14")


def _wait_for_judges(drv, mock, count, timeout=8.0):
    """The judge call lands at run end, after the reply is on screen — poll
    the wire (draining the tty meanwhile) until `count` judge requests."""
    end = time.time() + timeout
    while time.time() < end:
        if len(mock.requests_of("judge")) >= count:
            return True
        drv.drain(0.1)
    return len(mock.requests_of("judge")) >= count


# ── 15 wire invariant + cross-endpoint ──────────────────────────────────


def s15_wire_invariant():
    """The resolver is the wire-invariant gatekeeper: tier tokens die at the
    seam, real names pass through, and a cross-endpoint tier writes no
    override (the child runs its own client's boot model) while a persistent
    notice says so. Every spawn lands one audit line with the real name.
    """
    specs = (
        ("a", "MARK15A", "lite", "lite-alpha"),
        ("b", "MARK15B", "main", "mock-model"),
        ("c", "MARK15C", "wire-passthrough-x", "wire-passthrough-x"),
        ("d", "MARK15D", "advanced", "mock-model"),
    )
    mock = MockLLM()
    mock.child_marks = [mark for _, mark, _, _ in specs]
    script = []
    for tag, mark, model, _want in specs:
        script += spawn_turn(mark, f"WIRE-{tag.upper()}-DONE",
                             f"WIRE-{tag.upper()}-ACK",
                             model=model, task_name=tag)
    mock.set_script(script)
    # `advanced` is Full on a foreign endpoint: the cross-endpoint fallback's
    # fixture. `main`/`lite` share the boot endpoint (the mock).
    world, drv, ok = boot("15", mock, {
        "lite": "lite-alpha",
        "advanced": {"model": "adv-remote", "base_url": "http://127.0.0.1:9",
                     "api_key": "k-remote", "protocol": "openai"},
    })
    if not expect("15", "boot", ok, drv.screen.text()[-300:]):
        drv.quit()
        return

    for tag, mark, _model, want in specs:
        done, visible = run_spawn_turn(
            drv, f"go {tag}", f"WIRE-{tag.upper()}-DONE", f"WIRE-{tag.upper()}-ACK")
        on, got = child_ran_on(mock, mark, want)
        expect("15", f"spawn {tag} completes", done and visible,
               drv.dump(world, f"15-{tag}"))
        expect("15", f"spawn {tag} child body is {want}",
               on, f"child bodies: {got}")

    # The notice for the cross-endpoint spawn (d) is a persistent line.
    expect("15", "the cross-endpoint fallback says so on screen",
           screen_has(drv.screen, "no override written"),
           drv.dump(world, "15-notice"))

    # The wire invariant sweep: no request body ever carries a tier token.
    with mock.lock:
        bodies = [r.get("model") for r in mock.requests]
    leaked = [m for m in bodies if m in TIER_TOKENS]
    expect("15", "no tier token ever reaches body[model]", not leaked,
           f"leaked: {leaked}")

    # The audit account: one line per spawn, real names only, the
    # cross-endpoint line honest about the fallback.
    lines = audit_lines(world)
    expect("15", "the audit logs one line per spawn", len(lines) == 4,
           f"lines: {lines}")
    bad = [ln for ln in lines if ln.get("resolved_name") in TIER_TOKENS]
    expect("15", "the audit records real names, never tokens", not bad,
           f"token rows: {bad}")
    cross = [ln for ln in lines if ln.get("requested") == "advanced"]
    expect("15", "the cross-endpoint line says endpoint_ok=false",
           len(cross) == 1 and cross[0].get("endpoint_ok") is False
           and cross[0].get("resolved_name") == "adv-remote",
           f"cross rows: {cross}")
    passthrough = [ln for ln in lines if ln.get("requested") == "wire-passthrough-x"]
    expect("15", "a real name passes through, unclassified",
           len(passthrough) == 1 and passthrough[0].get("resolved_tier") is None
           and passthrough[0].get("resolved_name") == "wire-passthrough-x",
           f"passthrough rows: {passthrough}")
    finish(drv, world, "15")
