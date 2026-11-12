"""Detection probes. Each returns a flat dict of scored fields plus raw evidence.

Probes only observe: they never click, type into, or otherwise solve a challenge.
Reading a detector's own result object happens after its scan has finished, via a
DOM bridge (a <script> element that copies the result into a data attribute), so
the same code path works for every engine and cannot influence the scan.
"""

from __future__ import annotations

import hashlib
import io
import json
import re
import time
from pathlib import Path
from typing import Any, Callable

# --------------------------------------------------------------------------- helpers


def _poll(fn: Callable[[], Any], timeout: float, interval: float = 1.0) -> Any:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        try:
            last = fn()
            if last:
                return last
        except Exception:
            pass
        time.sleep(interval)
    return last


def _open(s, url: str) -> None:
    """Navigate and fail loudly unless the tab actually reached `url`'s host."""
    from urllib.parse import urlparse
    s.goto(url)
    want = urlparse(url).hostname
    href = _poll(lambda: (lambda h: h if urlparse(h).hostname == want else None)(s.evaluate("location.href")),
                 timeout=15, interval=1)
    if not href:
        raise RuntimeError(f"navigation to {want} did not complete (at {s.evaluate('location.href')})")


def _text(s) -> str:
    return s.evaluate("document.body ? document.body.innerText : ''") or ""


def main_world_json(s, expr: str) -> Any:
    """Evaluate `expr` in the page's main world after the fact, via a DOM bridge."""
    import secrets
    attr = "data-wsb-" + secrets.token_hex(6)
    inner = ("try { document.documentElement.setAttribute(%s, JSON.stringify((() => (%s))())); }"
             " catch (e) { document.documentElement.setAttribute(%s, JSON.stringify({__error: String(e)})); }"
             % (json.dumps(attr), expr, json.dumps(attr)))
    js = ("(() => { const el = document.createElement('script'); el.textContent = %s;"
          " document.documentElement.appendChild(el); el.remove();"
          " const v = document.documentElement.getAttribute(%s);"
          " document.documentElement.removeAttribute(%s); return v; })()"
          % (json.dumps(inner), json.dumps(attr), json.dumps(attr)))
    raw = s.evaluate(js)
    return json.loads(raw) if raw else None


# --------------------------------------------------------------------------- fingerprint

COLLECTOR_JS = r"""
(async () => {
  const sha = async (s) => {
    const b = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(s));
    return [...new Uint8Array(b)].map(x => x.toString(16).padStart(2, '0')).join('').slice(0, 16);
  };
  const out = {};
  const n = navigator;
  out.ua = n.userAgent; out.platform = n.platform; out.language = n.language;
  out.languages = (n.languages || []).join(','); out.cores = n.hardwareConcurrency;
  out.memory = n.deviceMemory ?? null; out.webdriver = n.webdriver; out.vendor = n.vendor;
  out.touch = n.maxTouchPoints;
  out.screen = [screen.width, screen.height, screen.availWidth, screen.availHeight, screen.colorDepth, devicePixelRatio].join('x');
  out.window = [outerWidth, outerHeight].join('x');
  const ro = Intl.DateTimeFormat().resolvedOptions();
  out.timezone = ro.timeZone; out.intl_locale = ro.locale; out.tz_offset = new Date().getTimezoneOffset();
  try {
    const uad = n.userAgentData;
    if (uad) {
      const h = await uad.getHighEntropyValues(['platformVersion', 'fullVersionList', 'architecture', 'bitness']);
      out.uad = JSON.stringify({brands: uad.brands, platform: uad.platform, h});
    }
  } catch (e) { out.uad = 'err'; }
  try {
    const c = document.createElement('canvas'); c.width = 280; c.height = 60;
    const x = c.getContext('2d');
    x.textBaseline = 'top'; x.font = "16px 'Arial'"; x.fillStyle = '#f60'; x.fillRect(100, 1, 62, 20);
    x.fillStyle = '#069'; x.fillText('Cwm fjordbank glyphs vext quiz \u{1F600}', 2, 15);
    x.fillStyle = 'rgba(102,204,0,0.7)'; x.fillText('Cwm fjordbank glyphs vext quiz', 4, 17);
    x.beginPath(); x.arc(50, 30, 20, 0, Math.PI * 2); x.stroke();
    out.canvas = await sha(c.toDataURL());
  } catch (e) { out.canvas = 'err'; }
  try {
    const c = document.createElement('canvas');
    const gl = c.getContext('webgl');
    if (gl) {
      const d = gl.getExtension('WEBGL_debug_renderer_info');
      out.webgl_vendor = d ? gl.getParameter(d.UNMASKED_VENDOR_WEBGL) : gl.getParameter(gl.VENDOR);
      out.webgl_renderer = d ? gl.getParameter(d.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
      out.webgl_params = await sha(JSON.stringify([gl.MAX_TEXTURE_SIZE, gl.MAX_VIEWPORT_DIMS, gl.ALIASED_LINE_WIDTH_RANGE].map(p => [...[].concat(gl.getParameter(p))])) + (gl.getSupportedExtensions() || []).join());
    } else { out.webgl_renderer = null; }
  } catch (e) { out.webgl_renderer = 'err'; }
  try {
    const ctx = new OfflineAudioContext(1, 5000, 44100);
    const o = ctx.createOscillator(); o.type = 'triangle'; o.frequency.value = 1e4;
    const comp = ctx.createDynamicsCompressor();
    o.connect(comp); comp.connect(ctx.destination); o.start(0);
    const buf = await ctx.startRendering();
    const d = buf.getChannelData(0); let sum = 0;
    for (let i = 4500; i < 5000; i++) sum += Math.abs(d[i]);
    out.audio = sum.toFixed(8);
  } catch (e) { out.audio = 'err'; }
  const fonts = ['Arial', 'Helvetica', 'Times New Roman', 'Courier New', 'Verdana', 'Segoe UI', 'Calibri',
    'Roboto', 'Ubuntu', 'DejaVu Sans', 'Liberation Sans', 'Noto Sans', 'Menlo', 'Monaco', 'SF Pro Text', 'Cantarell'];
  out.fonts = fonts.filter(f => document.fonts.check(`12px "${f}"`)).join(',');
  const keys = Object.keys(out).sort();
  out.fp_hash = await sha(JSON.stringify(keys.map(k => [k, out[k]])));
  return out;
})()
"""

# Fields expected to vary with the seed (identity) vs. the host (hardware).
FINGERPRINT_FIELDS = ["ua", "platform", "languages", "cores", "memory", "screen", "window", "timezone",
                      "intl_locale", "canvas", "webgl_renderer", "webgl_params", "audio", "fonts"]


def main_world_async(s, expr: str, timeout: float = 40) -> Any:
    """Run an async expression in the main world; result handed back via a DOM attribute."""
    import secrets
    attr = "data-wsb-" + secrets.token_hex(6)
    inner = ("Promise.resolve(%s).then(v => document.documentElement.setAttribute(%s, JSON.stringify(v)),"
             " e => document.documentElement.setAttribute(%s, JSON.stringify({__error: String(e)})));"
             % (expr, json.dumps(attr), json.dumps(attr)))
    s.evaluate("(() => { const el = document.createElement('script'); el.textContent = %s;"
               " document.documentElement.appendChild(el); el.remove(); return true; })()" % json.dumps(inner))
    raw = _poll(lambda: s.evaluate("document.documentElement.getAttribute(%s)" % json.dumps(attr)),
                timeout=timeout, interval=0.5)
    return json.loads(raw) if raw else None


def probe_fingerprint(s, shots: Path) -> dict[str, Any]:
    # Runs on a neutral page we do not score, in the main world: Firefox's isolated
    # world forbids typed-array reads over Xrays, so isolated-world hashing fails there.
    _open(s, "https://example.com/")
    fp = main_world_async(s, COLLECTOR_JS) or {}
    return {"fingerprint": fp, "fp_hash": fp.get("fp_hash")}


# --------------------------------------------------------------------------- CreepJS

CREEP_URL = "https://abrahamjuliot.github.io/creepjs/"
CREEP_EXPR = """(() => { const f = window.Fingerprint; if (!f || !f.lies) return null;
  const h = f.headless || {};
  return { lies: f.lies.totalLies, lie_apis: Object.keys(f.lies.data || {}),
    trash: f.trash ? f.trash.totalTrashValues : null,
    errors: f.capturedErrors && f.capturedErrors.data ? f.capturedErrors.data.length : null,
    like_headless: h.likeHeadlessRating, headless: h.headlessRating, stealth: h.stealthRating,
    worker_lang: f.workerScope ? f.workerScope.language : null,
    worker_cores: f.workerScope ? f.workerScope.hardwareConcurrency : null,
    worker_ua: f.workerScope ? f.workerScope.userAgent : null }; })()"""


def probe_creepjs(s, shots: Path) -> dict[str, Any]:
    _open(s, CREEP_URL)
    res = _poll(lambda: main_world_json(s, CREEP_EXPR), timeout=60, interval=2)
    time.sleep(2)
    text = _text(s)
    m = re.search(r"FP ID:\s*([0-9a-f]{16,})", text)
    s.screenshot(shots / "creepjs.png")
    out: dict[str, Any] = {"creep_ok": bool(res and "lies" in res), "creep_fpid": m.group(1) if m else None}
    if res and "lies" in res:
        out.update({f"creep_{k}": v for k, v in res.items()})
    return out


# --------------------------------------------------------------------------- BrowserScan

BSCAN_URL = "https://www.browserscan.net/bot-detection"
BSCAN_VERDICTS = ("Normal", "Robot", "Abnormal", "Warning")


def probe_browserscan(s, shots: Path) -> dict[str, Any]:
    _open(s, BSCAN_URL)

    def done():
        t = _text(s)
        m = re.search(r"Test Results:\s*\n+\s*(\w+)", t)
        return m.group(1) if m and m.group(1) in BSCAN_VERDICTS else None

    verdict = _poll(done, timeout=60, interval=2)
    time.sleep(2)
    t = _text(s)
    lines = [l.strip() for l in t.splitlines() if l.strip()]
    flagged = sorted({lines[i - 1] for i, l in enumerate(lines)
                      if i and l in ("Robot", "Abnormal", "Warning") and lines[i - 1] != "Test Results:"})
    s.screenshot(shots / "browserscan.png")
    return {"bscan_verdict": verdict, "bscan_flagged": flagged}


# --------------------------------------------------------------------------- pixelscan

PIXEL_URL = "https://pixelscan.net/fingerprint-check"
PIXEL_LABELS = ("Location", "Proxy", "Fingerprint", "Bot check")


def probe_pixelscan(s, shots: Path) -> dict[str, Any]:
    _open(s, PIXEL_URL)
    _poll(lambda: re.search(r"Your Browser Fingerprint is (\w+)", _text(s))
          and "Collecting Data" not in _text(s), timeout=120, interval=3)
    time.sleep(2)
    t = _text(s)
    m = re.search(r"Your Browser Fingerprint is (\w+)", t)
    lines = [l.strip() for l in t.splitlines() if l.strip()]
    fields: dict[str, str] = {}
    for i, l in enumerate(lines):
        if l in PIXEL_LABELS and i and l not in fields:
            prev = lines[i - 1]
            if not prev.startswith("Best ") and prev not in PIXEL_LABELS:
                fields[l] = prev
    s.screenshot(shots / "pixelscan.png")
    return {
        "pixel_consistent": (m.group(1).lower() == "consistent") if m else None,
        "pixel_fingerprint": fields.get("Fingerprint"),
        "pixel_proxy": fields.get("Proxy"),
        "pixel_bot": fields.get("Bot check"),
        "pixel_location": fields.get("Location"),
    }


# --------------------------------------------------------------------------- Turnstile

TS_WIDGET_URL = "https://peet.ws/turnstile-test/managed.html"
TS_PAGE_URL = "https://nopecha.com/demo/cloudflare"

WIDGET_RECT_JS = """(() => {
  const el = document.querySelector('.cf-turnstile, #cf-turnstile, [id^="cf-chl-widget"]')
    || [...document.querySelectorAll('div')].find(d => d.shadowRoot === null && d.querySelector('input[name="cf-turnstile-response"]'));
  const host = el && (el.querySelector('div') || el);
  const r = (host || el) ? (host || el).getBoundingClientRect() : null;
  const tok = document.querySelector('input[name="cf-turnstile-response"]');
  return { rect: r && r.width ? [r.x, r.y, r.width, r.height, devicePixelRatio] : null,
           token: tok ? tok.value.length : null, title: document.title,
           text: (document.body ? document.body.innerText : '').slice(0, 600) };
})()"""


def _widget_colour(png: bytes, rect) -> dict[str, float] | None:
    """Fraction of saturated red/green pixels inside the widget box.

    Turnstile draws a green check on success and a red icon on failure; the
    interactive checkbox state has neither. Language-independent by design.
    """
    if not rect:
        return None
    from PIL import Image
    im = Image.open(io.BytesIO(png)).convert("RGB")
    x, y, w, h, dpr = rect
    box = (int(x * dpr), int(y * dpr), int((x + w) * dpr), int((y + h) * dpr))
    crop = im.crop(box)
    px = list(crop.get_flattened_data() if hasattr(crop, 'get_flattened_data') else crop.getdata()) or [(0, 0, 0)]
    red = sum(1 for r, g, b in px if r > 170 and g < 90 and b < 90) / len(px)
    green = sum(1 for r, g, b in px if g > 100 and r < 90 and b < 120 and g - r > 50) / len(px)
    return {"red": round(red, 4), "green": round(green, 4)}


def _classify_widget(st: dict | None, colour: dict | None) -> str:
    if st and st.get("token"):
        return "pass"
    if not st or not st.get("rect"):
        return "blocked"  # widget never rendered
    if colour and colour["red"] > 0.002:
        return "blocked"
    if colour and colour["green"] > 0.004:
        return "pass"  # success drawn but token not yet propagated
    return "interactive"


def probe_turnstile_widget(s, shots: Path, wait: float = 30) -> dict[str, Any]:
    _open(s, TS_WIDGET_URL)
    st = _poll(lambda: (lambda v: v if v and v.get("token") else None)(s.evaluate(WIDGET_RECT_JS)),
               timeout=wait, interval=1)
    st = st or s.evaluate(WIDGET_RECT_JS)
    path = shots / "turnstile-widget.png"
    s.screenshot(path)
    colour = _widget_colour(path.read_bytes(), st.get("rect") if st else None)
    return {"ts_widget": _classify_widget(st, colour), "ts_widget_colour": colour,
            "ts_widget_token_len": st.get("token") if st else None}


CHALLENGE_MARKERS = ("Just a moment", "security verification", "Checking your browser",
                     "Sicherheitsüberprüfung", "安全驗證", "安全验证")
BLOCK_MARKERS = ("you have been blocked", "Access denied", "Error 1020", "Error 1015")


NAV_STATUS_JS = """(() => { const n = performance.getEntriesByType('navigation')[0];
  return { status: n ? n.responseStatus : null, title: document.title, href: location.href,
           chl: !!document.querySelector('#challenge-form, #challenge-stage, #challenge-error-text, [id^="cf-chl"], input[name="cf-turnstile-response"]'),
           text: (document.body ? document.body.innerText : '').slice(0, 400) }; })()"""


def probe_turnstile_page(s, shots: Path, wait: float = 40) -> dict[str, Any]:
    """Full-page Cloudflare managed challenge ("Just a moment...").

    pass        origin served (navigation status 2xx, no challenge DOM) without interaction
    interactive challenge still up, widget waiting for a human
    blocked     403 block page / widget failure

    Language-independent: decided from the navigation response status and the
    challenge DOM, not from localized text.
    """
    _open(s, TS_PAGE_URL)

    def cleared():
        v = s.evaluate(NAV_STATUS_JS)
        return v if v and v.get("status") and 200 <= v["status"] < 300 and not v.get("chl") else None

    st = _poll(cleared, timeout=wait, interval=1.5)
    final = st or s.evaluate(NAV_STATUS_JS) or {}
    path = shots / "turnstile-page.png"
    s.screenshot(path)
    blob = (final.get("title", "") + "\n" + final.get("text", "")).lower()
    if st:
        outcome = "pass"
    elif any(m.lower() in blob for m in BLOCK_MARKERS) and not final.get("chl"):
        outcome = "blocked"
    else:
        colour = _widget_colour_page(path.read_bytes())
        outcome = "blocked" if colour and colour["red"] > 0.0004 else "interactive"
    return {"ts_page": outcome, "ts_page_status": final.get("status"), "ts_page_title": final.get("title")}


def _widget_colour_page(png: bytes) -> dict[str, float]:
    # The interstitial centres a fixed-width column; scan its upper half.
    from PIL import Image
    im = Image.open(io.BytesIO(png)).convert("RGB")
    w, h = im.size
    crop = im.crop((int(w * 0.2), 0, int(w * 0.8), int(h * 0.6)))
    px = list(crop.get_flattened_data() if hasattr(crop, 'get_flattened_data') else crop.getdata())
    red = sum(1 for r, g, b in px if r > 170 and g < 90 and b < 90) / max(len(px), 1)
    return {"red": round(red, 5)}


PROBES: list[tuple[str, Callable]] = [
    ("fingerprint", probe_fingerprint),
    ("creepjs", probe_creepjs),
    ("browserscan", probe_browserscan),
    ("pixelscan", probe_pixelscan),
    ("turnstile_widget", probe_turnstile_widget),
    ("turnstile_page", probe_turnstile_page),
]


def sha16(obj: Any) -> str:
    return hashlib.sha256(json.dumps(obj, sort_keys=True).encode()).hexdigest()[:16]
