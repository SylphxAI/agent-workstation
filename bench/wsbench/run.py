"""Matrix runner: engines x egresses x runs. Writes one JSON line per run.

    python -m wsbench.run --out results/ --engine fpc --engine camoufox \\
        --egress residential=socks5://127.0.0.1:1080 --egress direct \\
        --seeds A,A,A,A,A,B,C,D --parallel 2

Each run gets a fresh virtual desktop sized to the seed's screen and a fresh
profile, then visits every probe in order. Screenshots land next to results.
"""

from __future__ import annotations

import argparse
import concurrent.futures as cf
import datetime as dt
import json
import threading
import time
import traceback
from pathlib import Path

from .display import Desktop, WaylandDesktop
from .engines import ENGINES, Egress, open_session, plan_launch
from .probes import PROBES

_lock = threading.Lock()


def parse_egress(spec: str) -> Egress:
    name, _, url = spec.partition("=")
    return Egress(name, url or None)


NET_ERROR_PREFIXES = ("chrome-error://", "about:neterror", "about:certerror")


def one_run(engine: str, egress: Egress, seed: str, idx: int, out: Path, display: str,
            probes: list[str], max_attempts: int = 4) -> dict:
    """Run once; if the egress dropped during the run, record it as discarded and retry."""
    for attempt in range(1, max_attempts + 1):
        rec = _attempt(engine, egress, seed, idx, out, display, probes, attempt)
        if rec["egress_ok"]:
            return rec
    return rec


def _attempt(engine: str, egress: Egress, seed: str, idx: int, out: Path, display: str,
             probes: list[str], attempt: int) -> dict:
    egress.wait_alive()
    try:  # the exit node may rotate; record the identity this run actually used
        egress.lookup()
    except Exception:
        pass
    run_id = f"{engine}.{egress.name}.{seed}.{idx:02d}"
    shots = out / "shots" / run_id
    shots.mkdir(parents=True, exist_ok=True)
    plan = plan_launch(engine, seed, egress)
    rec: dict = {
        "run_id": run_id, "engine": engine, "egress": egress.name, "egress_ip": egress.ip,
        "egress_country": egress.country, "egress_org": egress.org, "seed": seed, "display": display,
        "screen": list(plan.screen), "locale": plan.locale, "timezone": plan.timezone,
        "started": dt.datetime.now(dt.UTC).isoformat(timespec="seconds"), "errors": {},
        "attempt": attempt, "net_errors": [],
    }
    desk_cls = WaylandDesktop if display == "wayland" else Desktop
    t0 = time.time()
    try:
        with desk_cls(*plan.screen) as desk:
            sess = open_session(plan, egress, desk.env(), out / "state")
            try:
                if hasattr(sess, "config_digest"):
                    rec["camoufox_config"] = sess.config_digest
                for name, fn in PROBES:
                    if probes and name not in probes:
                        continue
                    try:
                        rec.update(fn(sess, shots))
                    except Exception as e:  # keep going; record the failure
                        rec["errors"][name] = f"{type(e).__name__}: {e}"[:400]
                    try:
                        href = sess.evaluate("location.href") or ""
                        if href.startswith(NET_ERROR_PREFIXES):
                            rec["net_errors"].append(name)
                    except Exception:
                        pass
            finally:
                sess.close()
    except Exception as e:
        rec["errors"]["launch"] = traceback.format_exc()[-800:]
    rec["seconds"] = round(time.time() - t0, 1)
    rec["egress_ok"] = not rec["net_errors"] and egress.alive() and "launch" not in rec["errors"]
    rec["discarded"] = not rec["egress_ok"]
    with _lock, open(out / "runs.jsonl", "a") as f:
        f.write(json.dumps(rec, default=str) + "\n")
    print(f"[{rec['started']}] {run_id} {rec['seconds']}s "
          f"ts_widget={rec.get('ts_widget')} ts_page={rec.get('ts_page')} "
          f"lies={rec.get('creep_lies')} bscan={rec.get('bscan_verdict')} "
          f"errors={list(rec['errors'])} net={rec['net_errors']} ok={rec['egress_ok']}", flush=True)
    return rec


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--engine", action="append", choices=ENGINES)
    ap.add_argument("--egress", action="append", default=[],
                    help="NAME or NAME=PROXY_URL (e.g. residential=socks5://127.0.0.1:1080); bare NAME = direct")
    ap.add_argument("--seeds", default="A,A,A,A,A,B,C,D",
                    help="comma list, one launch each; repeats measure stability, distinct values distinctness")
    ap.add_argument("--display", choices=("wayland", "x11"), default="wayland")
    ap.add_argument("--parallel", type=int, default=1)
    ap.add_argument("--probe", action="append", default=[], help="restrict to these probes")
    a = ap.parse_args()

    a.out.mkdir(parents=True, exist_ok=True)
    engines = a.engine or list(ENGINES[:2])
    egresses = [parse_egress(e).lookup() for e in (a.egress or ["direct"])]
    for e in egresses:
        print(f"egress {e.name}: {e.ip} {e.country} {e.org} {e.timezone}", flush=True)
    seeds = [s.strip() for s in a.seeds.split(",") if s.strip()]
    # Interleave engines so every engine sees the same time window per egress.
    jobs = [(eng, eg, seed, i) for eg in egresses for i, seed in enumerate(seeds) for eng in engines]
    with cf.ThreadPoolExecutor(max_workers=a.parallel) as pool:
        futs = [pool.submit(one_run, eng, eg, seed, i, a.out, a.display, a.probe) for eng, eg, seed, i in jobs]
        for f in cf.as_completed(futs):
            f.result()


if __name__ == "__main__":
    main()
