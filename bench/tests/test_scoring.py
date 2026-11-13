"""Offline tests for the scoring and planning logic (no browser, no network)."""

import io
import json

from PIL import Image

from wsbench import report
from wsbench.engines import Egress, plan_launch, render_baseline_script, baseline_fingerprint, seed_value
from wsbench.probes import _classify_widget, _widget_colour


def _png(colour, size=(100, 60)):
    buf = io.BytesIO()
    Image.new("RGB", size, colour).save(buf, "PNG")
    return buf.getvalue()


RECT = [0, 0, 100, 60, 1]


def test_widget_pass_on_token():
    assert _classify_widget({"token": 812, "rect": RECT}, None) == "pass"


def test_widget_interactive_without_token_or_colour():
    colour = _widget_colour(_png((245, 245, 245)), RECT)
    assert _classify_widget({"token": 0, "rect": RECT}, colour) == "interactive"


def test_widget_blocked_on_red_failure_icon():
    colour = _widget_colour(_png((222, 19, 3)), RECT)
    assert _classify_widget({"token": 0, "rect": RECT}, colour) == "blocked"


def test_widget_blocked_when_never_rendered():
    assert _classify_widget({"token": None, "rect": None}, None) == "blocked"


def test_seed_is_deterministic_and_distinct():
    assert seed_value("A") == seed_value("A")
    assert seed_value("A") != seed_value("B")


def test_plan_follows_egress_and_seed():
    eg = Egress("x", None, country="DE", timezone="Europe/Berlin")
    a1, a2, b = plan_launch("fpc", "A", eg), plan_launch("fpc", "A", eg), plan_launch("fpc", "B", eg)
    assert a1 == a2
    assert a1.extra["fp_seed"] != b.extra["fp_seed"]
    assert (a1.locale, a1.timezone) == ("de-DE", "Europe/Berlin")


def test_baseline_script_has_no_unfilled_placeholders(monkeypatch):
    monkeypatch.setattr("wsbench.engines.chrome_version", lambda: ("150", "150.0.0.1"))
    src = render_baseline_script(baseline_fingerprint("A"))
    import re
    code = "\n".join(l for l in src.splitlines() if not l.lstrip().startswith("//"))
    assert not re.search(r"__[A-Z]+__", code)
    assert '"zh-HK"' in code and "37445" in code


def test_report_excludes_discarded_runs(tmp_path):
    base = {"egress": "r", "engine": "e", "seed": "A", "ts_widget": "interactive", "ts_page": "pass",
            "creep_lies": 0, "bscan_verdict": "Normal", "fingerprint": {"ua": "x"}, "fp_hash": "h", "creep_fpid": "f"}
    rows = [dict(base, run_id=str(i)) for i in range(5)]
    rows.append(dict(base, run_id="bad", discarded=True, ts_page="blocked"))
    p = tmp_path / "runs.jsonl"
    p.write_text("\n".join(json.dumps(r) for r in rows))
    table = report.detection_table(report.load([str(p)]))
    assert "| r | e | 5 (1) | 0 / 5 / 0 | 5 / 0 / 0 |" in table
    stab = report.stability_table(report.load([str(p)]))
    assert "| r | e | 5 | yes | yes | none |" in stab
