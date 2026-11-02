#!/usr/bin/env python3
"""T10 — perf A/B gate (design §7.4, Issue 4.1 / O2): 前后对比制.

Runs the SAME scripted event sequence against two phimint binaries — the
pre-sink build ("before") and the shell-sunk build ("after") — harvesting each
run's `perf.log` (frame CSV) plus driver-measured phase wall times.

Fail criterion (O2 定案):
    any single operation  > 1.5x   -> FAIL (unless exempted)
    total replay duration > 1.2x   -> FAIL (unless exempted)
Exemptions are written rationales, recorded in
`notes/phi-agent-tui-t10-perf.md` (原因与量级) — pass them here with
`--exempt phase1,phase2` so the gate output reflects the ledger.

Noise policy: runs alternate before/after (spread machine drift) and the
comparison takes the MIN per phase across runs — min is the noise-robust
estimator of "how fast can this go". Frame aggregates (dirty frames, draw_ms
sums) are reported as supporting numbers (§7.4: 帧数/帧时长), gated only by O2.

Usage:
    python3 scripts/perf_ab.py                      # default binaries, 3 runs
    python3 scripts/perf_ab.py --runs 5 --exempt copy
    python3 scripts/perf_ab.py --before /path/b --after /path/a
"""

import argparse
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "pty_smoke"))
from harness import Driver, MockLLM, World, boot_ok, spawn  # noqa: E402

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
AFTER_DEFAULT = os.path.join(ROOT, "target", "debug", "phimint")
BEFORE_DEFAULT = "/tmp/phimint-ab-before/target/debug/phimint"

PHASE_FAIL = 1.5    # single operation (O2)
TOTAL_FAIL = 1.2    # replay total (O2)


def settle(drv, timeout=4.0):
    """Wait until two consecutive drains yield the same screen — the redraw
    has finished. Measuring a fixed drain would bake the constant into the
    ratio and hide reflow cost."""
    prev = None
    end = time.time() + timeout
    while time.time() < end:
        drv.drain(0.15)
        text = drv.screen.text()
        if text == prev:
            return True
        prev = text
    return False


def perf_aggregates(perf_path):
    """Sum the frame CSV: dirty-frame count and draw/capture/loop ms sums."""
    agg = {"dirty_frames": 0, "draw_ms": 0, "capture_ms": 0, "loop_ms": 0}
    if not os.path.isfile(perf_path):
        return agg
    with open(perf_path) as fh:
        header = fh.readline().strip().split(",")
        idx = {name: i for i, name in enumerate(header)}
        for line in fh:
            cols = line.rstrip("\n").split(",")
            if len(cols) != len(header):
                continue
            if cols[idx["dirty"]] != "true":
                continue
            agg["dirty_frames"] += 1
            for key in ("draw_ms", "capture_ms", "loop_ms"):
                agg[key] += int(cols[idx[key]])
    return agg


def run_sequence(binary, tag):
    """One replay. Returns (phases {name: seconds}, perf aggregates)."""
    mock = MockLLM()
    mock.set_script([
        {"chunks": ["chunk-%d " % i for i in range(12)], "gap": 0.05},
        {"text":
            "# Heading-MARK\n\n"
            "- item-alpha\n"
            "- select-alpha\n"
            "- select-beta\n"
            "- select-gamma\n\n"
            "Inline **bold-word** and `code-word`\n"
            + "\n".join(f"LINE-{i:03d}" for i in range(40)) + "\nTAIL-MARK"},
        {"text": "final-FINAL"},
    ])
    world = World(tag)
    phases = {}

    t0 = time.time()
    pid, fd, slave = spawn(binary, world, mock, f"perf-{tag}")
    drv = Driver(pid, fd, slave)
    if not boot_ok(drv):
        drv.quit()
        return None, None
    phases["boot"] = time.time() - t0

    with open(os.path.join(world.ws, "fixture-file.txt"), "w") as fh:
        fh.write("fixture\n")

    # turn_stream: paced streaming reply
    t0 = time.time()
    drv.type("stream it")
    drv.key(drv.ENTER)
    drv.wait_for(lambda s: s.contains("chunk-11"), 15.0)
    phases["turn_stream"] = time.time() - t0

    # turn_reply: markdown + selectable rows + scrollable length
    t0 = time.time()
    drv.type("render it")
    drv.key(drv.ENTER)
    drv.wait_for(lambda s: s.contains("TAIL-MARK"), 15.0)
    phases["turn_reply"] = time.time() - t0

    # scroll: page up to the head, page down to the tail
    s = drv.screen
    t0 = time.time()
    for _ in range(10):
        drv.key(drv.PG_UP)
        drv.drain(0.2)
        if s.contains("LINE-000"):
            break
    for _ in range(10):
        drv.key(drv.PG_DOWN)
        drv.drain(0.2)
        if s.contains("TAIL-MARK"):
            break
    phases["scroll"] = time.time() - t0

    # picker: slash open/Esc, mention open/Esc
    t0 = time.time()
    drv.type("/")
    drv.wait_for(lambda s: s.row_of("resume") is not None, 3.0)
    drv.key(drv.ESC)
    drv.drain(0.3)
    for _ in range(4):
        drv.key(b"\x7f")
    drv.type("@fixt")
    drv.wait_for(lambda s: s.contains("fixture-file"), 3.0)
    drv.key(drv.ESC)
    drv.drain(0.3)
    for _ in range(8):
        drv.key(b"\x7f")
    phases["picker"] = time.time() - t0

    # copy: page up until the select rows are visible (the 40-line tail below
    # them has scrolled them off-grid), then drag + right-click menu + Enter
    t0 = time.time()
    for _ in range(10):
        if s.row_of("select-alpha") is not None:
            break
        drv.key(drv.PG_UP)
        drv.drain(0.2)
    r = s.row_of("select-alpha")
    if r is not None:
        drv.mouse(0, 5, r, press=True)
        drv.mouse(32, 25, r + 1, press=True)
        drv.mouse(0, 25, r + 1, press=False)
        drv.drain(0.3)
        drv.mouse(2, 12, r, press=True)
        drv.mouse(2, 12, r, press=False)
        drv.drain(0.3)
        drv.key(drv.ENTER)
        drv.drain(0.3)
    phases["copy"] = time.time() - t0
    if r is None:
        print(f"[{tag}] WARN copy phase skipped — select rows not on grid")

    # resize: shrink and restore; settle() waits for the reflow to finish
    t0 = time.time()
    drv.resize(30, 100)
    settle(drv)
    drv.resize(40, 120)
    settle(drv)
    phases["resize"] = time.time() - t0

    # quit: Ctrl+C x2 + Ctrl+D incl. the exit replay
    t0 = time.time()
    drv.quit()
    phases["quit"] = time.time() - t0

    perf = perf_aggregates(os.path.join(world.sessions_dir(), f"perf-{tag}", "perf.log"))
    return phases, perf


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--before", default=BEFORE_DEFAULT)
    ap.add_argument("--after", default=AFTER_DEFAULT)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--exempt", default="")  # comma list of phase names
    ap.add_argument("--report", default="")  # JSON output path
    args = ap.parse_args()
    exempt = {p.strip() for p in args.exempt.split(",") if p.strip()}

    for name, path in (("before", args.before), ("after", args.after)):
        if not os.path.isfile(path):
            print(f"missing {name} binary: {path}")
            return 2

    results = {"before": [], "after": []}
    perfs = {"before": [], "after": []}
    for i in range(args.runs):
        # Alternate the order — a side that always runs first eats the cold
        # cache and flatters the other.
        order = (("before", args.before), ("after", args.after))
        if i % 2:
            order = tuple(reversed(order))
        for side, binary in order:
            tag = f"{side}{i}"
            phases, perf = run_sequence(binary, tag)
            if phases is None:
                print(f"[{side} run {i}] FAILED to complete — aborting")
                return 2
            results[side].append(phases)
            perfs[side].append(perf)
            total = sum(phases.values())
            print(f"[{side} run {i}] total {total:.2f}s " +
                  " ".join(f"{k}={v:.2f}" for k, v in phases.items()))

    # Min per phase across runs — the noise-robust point estimate.
    names = list(results["before"][0].keys())
    bmin = {k: min(r[k] for r in results["before"]) for k in names}
    amin = {k: min(r[k] for r in results["after"]) for k in names}
    b_total, a_total = sum(bmin.values()), sum(amin.values())

    print("\nphase          before    after    ratio  verdict")
    failed = []
    for k in names:
        ratio = amin[k] / bmin[k] if bmin[k] > 0 else float("inf")
        verdict = "ok"
        if ratio > PHASE_FAIL:
            verdict = "EXEMPT" if k in exempt else "FAIL"
            if verdict == "FAIL":
                failed.append(k)
        print(f"{k:13s} {bmin[k]:7.2f}s {amin[k]:7.2f}s {ratio:7.2f}  {verdict}")
    total_ratio = a_total / b_total if b_total > 0 else float("inf")
    total_verdict = "ok"
    if total_ratio > TOTAL_FAIL:
        total_verdict = "FAIL"
        failed.append("TOTAL")
    print(f"{'TOTAL':13s} {b_total:7.2f}s {a_total:7.2f}s {total_ratio:7.2f}  {total_verdict}")

    for side in ("before", "after"):
        agg = {k: min(p[k] for p in perfs[side]) for k in perfs[side][0]}
        agg_max = {k: max(p[k] for p in perfs[side]) for k in perfs[side][0]}
        print(f"frames[{side}] dirty(min-max)={agg['dirty_frames']}-{agg_max['dirty_frames']} "
              f"draw_ms(min)={agg['draw_ms']} capture_ms(min)={agg['capture_ms']}")

    report = {
        "runs": args.runs,
        "per_phase_min": {"before": bmin, "after": amin},
        "ratios": {k: (amin[k] / bmin[k] if bmin[k] > 0 else None) for k in names},
        "total": {"before": b_total, "after": a_total, "ratio": total_ratio},
        "exempt": sorted(exempt),
        "failed": failed,
        "perfs": perfs,
    }
    if args.report:
        with open(args.report, "w") as fh:
            json.dump(report, fh, indent=2)
        print(f"\nreport written: {args.report}")

    print("\n" + ("FAIL: " + ", ".join(failed) if failed else
                  "PASS (O2 thresholds hold" +
                  (f"; exemptions: {sorted(exempt)}" if exempt else "") + ")"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
