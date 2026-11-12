"""Aggregate runs.jsonl files into the markdown result tables.

    python -m wsbench.report results/runs.jsonl [more.jsonl ...]

Discarded runs (egress dropped mid-run) are counted but excluded from scores.
"""

from __future__ import annotations

import json
import statistics
import sys
from collections import Counter, defaultdict

from .probes import FINGERPRINT_FIELDS


def load(paths: list[str]) -> list[dict]:
    rows = []
    for p in paths:
        with open(p) as f:
            rows += [json.loads(l) for l in f if l.strip()]
    return rows


def _counts(vals: list, order: tuple) -> str:
    c = Counter(v for v in vals if v is not None)
    return " / ".join(str(c.get(k, 0)) for k in order)


def _med(vals: list) -> str:
    v = [x for x in vals if isinstance(x, (int, float))]
    if not v:
        return "–"
    return f"{statistics.median(v):g} ({min(v):g}–{max(v):g})"


def detection_table(rows: list[dict]) -> str:
    cells: dict[tuple, list[dict]] = defaultdict(list)
    discarded: Counter = Counter()
    for r in rows:
        key = (r["egress"], r["engine"])
        if r.get("discarded"):
            discarded[key] += 1
        else:
            cells[key].append(r)
    out = ["| egress | engine | N clean (discarded) | Turnstile widget pass/inter/blocked | "
           "Turnstile page pass/inter/blocked | CreepJS lies med (range) | CreepJS like-headless % | "
           "CreepJS headless % / stealth % | BrowserScan Normal | pixelscan fingerprint | pixelscan bot check |",
           "|---|---|---|---|---|---|---|---|---|---|---|"]
    for key in sorted(cells):
        rs = cells[key]
        n = len(rs)
        bscan = sum(1 for r in rs if r.get("bscan_verdict") == "Normal")
        bscan_n = sum(1 for r in rs if r.get("bscan_verdict"))
        pf = Counter(r.get("pixel_fingerprint") for r in rs if r.get("pixel_fingerprint")
                     and "Collecting" not in r.get("pixel_fingerprint", ""))
        pb = Counter(r.get("pixel_bot") for r in rs if r.get("pixel_bot")
                     and "Collecting" not in r.get("pixel_bot", ""))
        out.append("| " + " | ".join([
            key[0], key[1], f"{n} ({discarded.get(key, 0)})",
            _counts([r.get("ts_widget") for r in rs], ("pass", "interactive", "blocked")),
            _counts([r.get("ts_page") for r in rs], ("pass", "interactive", "blocked")),
            _med([r.get("creep_lies") for r in rs]),
            _med([r.get("creep_like_headless") for r in rs]),
            f"{_med([r.get('creep_headless') for r in rs])} / {_med([r.get('creep_stealth') for r in rs])}",
            f"{bscan}/{bscan_n}",
            "; ".join(f"{k} ×{v}" for k, v in pf.most_common()) or "–",
            "; ".join(f"{k} ×{v}" for k, v in pb.most_common()) or "–",
        ]) + " |")
    return "\n".join(out)


def stability_table(rows: list[dict]) -> str:
    """Same seed across launches -> identical; different seeds -> distinct."""
    by: dict[tuple, list[dict]] = defaultdict(list)
    for r in rows:
        if not r.get("discarded") and r.get("fingerprint"):
            by[(r["egress"], r["engine"])].append(r)
    out = ["| egress | engine | same-seed launches | identical collector hash | identical CreepJS FP ID | "
           "fields drifting within a seed | distinct seeds | distinct collector hashes | distinct CreepJS FP IDs | "
           "fields that vary across seeds |",
           "|---|---|---|---|---|---|---|---|---|---|"]
    for key in sorted(by):
        rs = by[key]
        seeds = Counter(r["seed"] for r in rs)
        main_seed = seeds.most_common(1)[0][0]
        same = [r for r in rs if r["seed"] == main_seed]
        drift = sorted({f for f in FINGERPRINT_FIELDS
                        if len({json.dumps(r["fingerprint"].get(f)) for r in same}) > 1})
        firsts = {}
        for r in rs:
            firsts.setdefault(r["seed"], r)
        vary = sorted({f for f in FINGERPRINT_FIELDS
                       if len({json.dumps(r["fingerprint"].get(f)) for r in firsts.values()}) > 1})
        out.append("| " + " | ".join([
            key[0], key[1], str(len(same)),
            "yes" if len({r.get('fp_hash') for r in same}) == 1 else f"no ({len({r.get('fp_hash') for r in same})} values)",
            "yes" if len({r.get('creep_fpid') for r in same}) == 1 else f"no ({len({r.get('creep_fpid') for r in same})} values)",
            ", ".join(drift) or "none",
            str(len(firsts)),
            str(len({r.get("fp_hash") for r in firsts.values()})),
            str(len({r.get("creep_fpid") for r in firsts.values()})),
            ", ".join(vary) or "none",
        ]) + " |")
    return "\n".join(out)


def lies_table(rows: list[dict]) -> str:
    by: dict[tuple, Counter] = defaultdict(Counter)
    flagged: dict[tuple, Counter] = defaultdict(Counter)
    for r in rows:
        if r.get("discarded"):
            continue
        k = (r["egress"], r["engine"])
        by[k].update(r.get("creep_lie_apis") or [])
        flagged[k].update(r.get("bscan_flagged") or [])
    out = ["| egress | engine | CreepJS lie APIs (runs) | BrowserScan flagged checks (runs) |", "|---|---|---|---|"]
    for k in sorted(set(by) | set(flagged)):
        out.append(f"| {k[0]} | {k[1]} | " + (", ".join(f"{a} ×{n}" for a, n in by[k].most_common()) or "none")
                   + " | " + (", ".join(f"{a} ×{n}" for a, n in flagged[k].most_common()) or "none") + " |")
    return "\n".join(out)


def identity_table(rows: list[dict]) -> str:
    """What each engine presents (first clean run per engine/egress)."""
    seen = {}
    for r in rows:
        if not r.get("discarded") and r.get("fingerprint"):
            seen.setdefault((r["egress"], r["engine"]), r)
    out = ["| egress | engine | UA | cores | memory | WebGL renderer | screen | languages | timezone |",
           "|---|---|---|---|---|---|---|---|---|"]
    for k in sorted(seen):
        f = seen[k]["fingerprint"]
        out.append("| " + " | ".join(str(x) for x in [k[0], k[1], f.get("ua"), f.get("cores"), f.get("memory"),
                                                      (f.get("webgl_renderer") or "")[:70], f.get("screen"),
                                                      f.get("languages"), f.get("timezone")]) + " |")
    return "\n".join(out)


def main() -> None:
    rows = load(sys.argv[1:])
    egress = sorted({(r["egress"], r.get("egress_ip"), r.get("egress_org")) for r in rows if not r.get("discarded")})
    print("Egress identities used:", "; ".join(f"{e[0]}={e[1]} ({e[2]})" for e in egress), "\n")
    print("### Detection\n")
    print(detection_table(rows), "\n")
    print("### What tripped\n")
    print(lies_table(rows), "\n")
    print("### Seed stability and distinctness\n")
    print(stability_table(rows), "\n")
    print("### Presented identity\n")
    print(identity_table(rows))


if __name__ == "__main__":
    main()
