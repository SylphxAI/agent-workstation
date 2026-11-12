"""Startup-time and memory probe: launch time and peak RSS per engine.

    python -m wsbench.perf --egress direct --seeds A,A --repeats 2

One fresh compositor + browser per measurement. Reports median/min/max of
launch seconds and peak RSS (MiB) per engine, then a machine-readable JSON
line per engine on stdout. Uses the same launch path as the detection run
(`plan_launch` / `open_session` / `WaylandDesktop`), no page hooks.
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import time
from pathlib import Path

import os

from .display import Desktop, WaylandDesktop
from .engines import ENGINES, Egress, open_session, plan_launch
from .run import parse_egress


def _rss_mib(pid: int) -> float:
    """RSS of a process tree rooted at pid, from /proc (Linux, MiB)."""
    try:
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    kb = float(line.split()[1])
                    break
            else:
                return 0.0
    except (OSError, ValueError):
        return 0.0
    total = kb / 1024
    try:
        children = subprocess.run(
            ["pgrep", "-P", str(pid)], capture_output=True, text=True, timeout=10).stdout.split()
    except Exception:
        children = []
    for c in children:
        total += _rss_mib(int(c))
    return total


def _root_pids(sess, plan_engine: str) -> list[int]:
    """Browser root pid(s) for an open session (used for RSS sampling)."""
    if plan_engine.startswith("chrome") or plan_engine.startswith("fpc"):
        try:
            return [sess.proc.pid]
        except AttributeError:
            return []
    # Camoufox is driven by Playwright from this python process; the browser
    # tree is its descendant, so sampling our own tree covers it (and the
    # compositor is outside both trees and excluded for every engine).
    return [os.getpid()]


def measure(engine: str, egress: Egress, seed: str, display: str,
            state: Path) -> dict:
    plan = plan_launch(engine, seed, egress)
    screen = plan.screen
    state.mkdir(parents=True, exist_ok=True)
    desk_cls = WaylandDesktop if display == "wayland" else Desktop
    t0 = time.time()
    with desk_cls(*screen) as desk:
        t_launch0 = time.time()
        sess = open_session(plan, egress, desk.env(), state)
        launch_s = time.time() - t_launch0
        pids = _root_pids(sess, engine)
        peak = 0.0
        try:
            # Sample RSS while the browser sits on a blank page for a few seconds.
            deadline = time.time() + 4
            while time.time() < deadline:
                m = sum(_rss_mib(p) for p in pids)
                peak = max(peak, m)
                time.sleep(0.5)
            sess.goto("about:blank", timeout=15)
            sess.screenshot(state / "perf-shot.png")
        finally:
            sess.close()
    return {"engine": engine, "egress": egress.name, "seed": seed,
            "launch_s": round(launch_s, 2), "peak_mib": round(peak, 1),
            "total_s": round(time.time() - t0, 1)}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--engine", action="append", choices=ENGINES)
    ap.add_argument("--egress", action="append", default=[],
                    help="NAME or NAME=PROXY_URL; bare NAME = direct")
    ap.add_argument("--seeds", default="A,A")
    ap.add_argument("--display", choices=("wayland", "x11"), default="wayland")
    ap.add_argument("--out", type=Path, default=Path("results-perf"))
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    engines = a.engine or ["fpc", "camoufox"]
    egresses = [parse_egress(e).lookup() for e in (a.egress or ["direct"])]
    seeds = [s.strip() for s in a.seeds.split(",") if s.strip()]
    rows = []
    for eg in egresses:
        for eng in engines:
            for seed in seeds:
                r = measure(eng, eg, seed, a.display, a.out / "state")
                r["at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
                rows.append(r)
                print(f"{eng}.{eg.name}.{seed} launch={r['launch_s']}s "
                      f"peak={r['peak_mib']}MiB total={r['total_s']}s", flush=True)
    with open(a.out / "perf.jsonl", "a") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    # Summary table (median; min-max in parens).
    print("\n| engine | launches | launch s med (min-max) | peak MiB med (min-max) |")
    print("|---|---|---|---|")
    for eg in egresses:
        for eng in engines:
            sub = [r for r in rows if r["engine"] == eng and r["egress"] == eg.name]
            ls = sorted(r["launch_s"] for r in sub)
            ms = sorted(r["peak_mib"] for r in sub)
            med = lambda v: f"{statistics.median(v):g} ({v[0]:g}-{v[-1]:g})"
            print(f"| {eng} | {len(sub)} | {med(ls)} | {med(ms)} |")


if __name__ == "__main__":
    main()
