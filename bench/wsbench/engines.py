"""Browser configurations under test.

Every engine exposes the same tiny surface used by the probes:
    goto(url), evaluate(js) -> JSON value, screenshot(path), close()

Extraction scripts run in an isolated world (CDP `Page.createIsolatedWorld`,
Playwright's default utility world for Camoufox) so the harness itself adds no
main-world globals or `Runtime.enable` side effects, except in the JS-hook
baseline, which reproduces the CDP call sequence such tools actually issue.
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .cdp import Cdp, CdpError

HERE = Path(__file__).resolve().parent

# Screen sizes ranked among the most common desktop resolutions; the seed picks one.
SCREENS = [
    (1920, 1080), (1366, 768), (1536, 864), (1440, 900), (1280, 720),
    (1600, 900), (2560, 1440), (1680, 1050), (1280, 800), (1920, 1200),
]

# Logical core counts common on consumer desktops; applied as a CPU affinity mask.
CORE_POOL = [4, 8, 12, 16]


def splitmix64(x: int) -> int:
    x = (x + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & 0xFFFFFFFFFFFFFFFF
    return x ^ (x >> 31)


def seed_value(seed: str) -> int:
    """FNV-1a + splitmix finaliser: stable 64-bit value for a profile seed string."""
    h = 0xCBF29CE484222325
    for b in seed.encode():
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return splitmix64(h)


@dataclass
class Egress:
    """Network identity for a run. `proxy` is a URL or None for direct."""

    name: str
    proxy: str | None
    ip: str = ""
    country: str = ""
    timezone: str = ""
    org: str = ""

    def alive(self, timeout: int = 8) -> bool:
        if not self.proxy:
            return True
        cmd = ["curl", "-sS", "-m", str(timeout), "-o", "/dev/null", "-w", "%{http_code}",
               "--proxy", self.proxy.replace("socks5://", "socks5h://"),
               "https://www.cloudflare.com/cdn-cgi/trace"]
        out = subprocess.run(cmd, capture_output=True, text=True)
        return out.stdout.strip() == "200"

    def wait_alive(self, max_wait: float = 1800, interval: float = 5) -> bool:
        deadline = time.time() + max_wait
        while time.time() < deadline:
            if self.alive():
                return True
            time.sleep(interval)
        return False

    def lookup(self) -> "Egress":
        if not self.wait_alive():
            raise RuntimeError(f"egress {self.name} unreachable for 30 min")
        cmd = ["curl", "-sS", "-m", "20", "https://ipinfo.io/json"]
        if self.proxy:
            cmd[1:1] = ["--proxy", self.proxy.replace("socks5://", "socks5h://")]
        out = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
        if out.returncode != 0:
            raise RuntimeError(f"egress {self.name} unreachable: {out.stderr.strip()}")
        info = json.loads(out.stdout)
        self.ip, self.country = info.get("ip", ""), info.get("country", "")
        self.timezone, self.org = info.get("timezone", "UTC"), info.get("org", "")
        return self


# Primary browser locale per egress country (small table; unknown -> en-US).
COUNTRY_LOCALE = {
    "US": "en-US", "GB": "en-GB", "CA": "en-CA", "AU": "en-AU", "IE": "en-IE", "NZ": "en-NZ",
    "DE": "de-DE", "AT": "de-AT", "CH": "de-CH", "FR": "fr-FR", "NL": "nl-NL", "BE": "nl-BE",
    "ES": "es-ES", "IT": "it-IT", "PT": "pt-PT", "BR": "pt-BR", "PL": "pl-PL", "SE": "sv-SE",
    "FI": "fi-FI", "DK": "da-DK", "NO": "nb-NO", "JP": "ja-JP", "KR": "ko-KR", "TW": "zh-TW",
    "HK": "zh-HK", "CN": "zh-CN", "SG": "en-SG", "IN": "en-IN",
}


def accept_languages(locale: str) -> list[str]:
    base = locale.split("-")[0]
    langs = [locale, base]
    if base != "en":
        langs += ["en-US", "en"]
    return langs


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def chrome_binary() -> str:
    for c in (os.environ.get("WSBENCH_CHROME"), "google-chrome-stable", "google-chrome"):
        if c and (shutil.which(c) or Path(c).is_file()):
            return shutil.which(c) or c
    raise RuntimeError("Google Chrome not found; set WSBENCH_CHROME")


@dataclass
class LaunchPlan:
    """Everything derived from (engine, seed, egress) before launch; recorded with results."""

    engine: str
    seed: str
    screen: tuple[int, int]
    locale: str
    timezone: str
    extra: dict[str, Any] = field(default_factory=dict)


# --------------------------------------------------------------------------- Chrome


class ChromeSession:
    """Headful Google Chrome over raw CDP.

    mode="seed":    real Chrome, launch-time identity only (screen via Xvfb, --lang,
                    TZ env, proxy). No JS hooks, no Emulation overrides, no Runtime.enable.
    mode="jshooks": baseline. Same binary, plus the flag set, Emulation overrides,
                    domain enables and document-start override script such tools ship.
    """

    def __init__(self, mode: str, plan: LaunchPlan, egress: Egress, env: dict[str, str]):
        assert mode in ("seed", "jshooks", "fpc", "fpc-brand")
        self.mode, self.plan = mode, plan
        self.tmp = Path(tempfile.mkdtemp(prefix="wsbench-chrome-"))
        self.port = _free_port()
        args = [
            fpc_binary() if mode.startswith("fpc") else chrome_binary(),
            f"--user-data-dir={self.tmp / 'profile'}",
            f"--remote-debugging-port={self.port}",
            "--remote-debugging-address=127.0.0.1",
            "--no-first-run",
            "--no-default-browser-check",
            "--password-store=basic",
            "--start-maximized",
            f"--lang={plan.locale}",
            # No GPU on the bench host; without this Chrome >=137 exposes no WebGL at all.
            "--enable-unsafe-swiftshader",
        ]
        if env.get("WAYLAND_DISPLAY"):
            args.append("--ozone-platform=wayland")
        if egress.proxy:
            args += [f"--proxy-server={egress.proxy}",
                     "--force-webrtc-ip-handling-policy=disable_non_proxied_udp"]
        if mode == "fpc-brand":
            # Same fork, but claim the current stable Chrome version in UA/client hints.
            args.append(f"--fingerprint-brand-version={chrome_version()[1]}")
        if mode.startswith("fpc"):
            # The fork's own launch-time identity: one integer seed drives canvas,
            # audio, fonts, client rects, GPU and hardware values in C++.
            args += [
                f"--fingerprint={plan.extra['fp_seed']}",
                "--fingerprint-brand=Chrome",
                f"--timezone={plan.timezone}",
                f"--accept-lang={','.join(accept_languages(plan.locale))}",
            ]
        if mode == "jshooks":
            fp = plan.extra["fp"]
            args += [
                "--remote-allow-origins=*",
                "--disable-blink-features=AutomationControlled",
                "--disable-infobars",
                "--use-mock-keychain",
                "--disable-background-timer-throttling",
                "--disable-backgrounding-occluded-windows",
                "--disable-renderer-backgrounding",
                "--disable-dev-shm-usage",
                "--no-sandbox",
                "--enable-webgl",
                "--ignore-gpu-blocklist",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                f"--user-agent={fp['user_agent']}",
                f"--window-size={fp['screen_width']},{fp['screen_height']}",
            ]
        # Hosts without unprivileged user namespaces cannot run Chrome's sandbox.
        if os.environ.get("WSBENCH_CHROME_NO_SANDBOX") == "1" and "--no-sandbox" not in args:
            args.append("--no-sandbox")
        args.append("about:blank")
        child_env = {**env, "TZ": plan.timezone}
        cpus = None
        if mode in ("seed", "fpc", "fpc-brand"):
            # Launch-time identity, no page hooks: Chrome derives navigator.language(s)
            # and Intl from the process locale and Accept-Language from the profile
            # preference; navigator.hardwareConcurrency follows the CPU affinity mask.
            base = plan.locale.split("-")[0]
            child_env.update(LANG=plan.locale.replace("-", "_") + ".UTF-8",
                             LANGUAGE=f"{plan.locale}:{base}")
            prefs = self.tmp / "profile" / "Default" / "Preferences"
            prefs.parent.mkdir(parents=True)
            prefs.write_text(json.dumps(
                {"intl": {"accept_languages": ",".join(accept_languages(plan.locale))}}))
            if mode == "seed":
                cpus = set(range(plan.extra["cores"]))
        self.log = open(self.tmp / "chrome.log", "wb")
        self.proc = subprocess.Popen(
            args, env=child_env, stdout=self.log, stderr=subprocess.STDOUT,
            preexec_fn=(lambda: os.sched_setaffinity(0, cpus)) if cpus else None)
        self.cdp = Cdp.from_port(self.port)
        targets = self.cdp.call("Target.getTargets")["targetInfos"]
        page = next(t for t in targets if t["type"] == "page")
        self.target_id = page["targetId"]
        self.sid = self.cdp.call("Target.attachToTarget",
                                 {"targetId": self.target_id, "flatten": True})["sessionId"]
        if mode == "jshooks":
            self._apply_baseline(plan.extra["fp"])

    def _apply_baseline(self, fp: dict[str, Any]) -> None:
        c, s = self.cdp, self.sid
        for d in ("Page.enable", "Runtime.enable", "DOM.enable", "Network.enable"):
            c.call(d, session=s)
        major, full = fp["chrome_major"], fp["chrome_full_version"]
        c.call("Emulation.setUserAgentOverride", {
            "userAgent": fp["user_agent"],
            "acceptLanguage": fp["accept_language"],
            "platform": "Linux x86_64",
            "userAgentMetadata": {
                "brands": [{"brand": "Not(A:Brand", "version": "99"},
                           {"brand": "Google Chrome", "version": major},
                           {"brand": "Chromium", "version": major}],
                "fullVersionList": [{"brand": "Not(A:Brand", "version": "99.0.0.0"},
                                    {"brand": "Google Chrome", "version": full},
                                    {"brand": "Chromium", "version": full}],
                "platform": "Linux", "platformVersion": "6.8.0", "architecture": "x86",
                "model": "", "mobile": False, "bitness": "64", "wow64": False,
            },
        }, session=s)
        c.call("Emulation.setTimezoneOverride", {"timezoneId": fp["timezone"]}, session=s)
        script = render_baseline_script(fp)
        c.call("Page.addScriptToEvaluateOnNewDocument", {"source": script}, session=s)
        c.call("Runtime.evaluate", {"expression": script, "returnByValue": True}, session=s)

    def goto(self, url: str, timeout: float = 45.0) -> None:
        try:
            r = self.cdp.call("Page.navigate", {"url": url}, session=self.sid, timeout=timeout)
        except CdpError:
            r = {}  # slow commit; the caller verifies where the tab ended up
        if r.get("errorText"):
            raise CdpError(f"navigate {url}: {r['errorText']}")
        deadline = time.time() + timeout
        time.sleep(0.5)
        while time.time() < deadline:
            try:
                if self.evaluate("document.readyState") == "complete":
                    return
            except CdpError:
                pass
            time.sleep(0.5)

    def evaluate(self, js: str, timeout: float = 30.0) -> Any:
        frame = self.cdp.call("Page.getFrameTree", session=self.sid)["frameTree"]["frame"]["id"]
        ctx = self.cdp.call("Page.createIsolatedWorld",
                            {"frameId": frame, "worldName": "wsbench", "grantUniveralAccess": False},
                            session=self.sid)["executionContextId"]
        r = self.cdp.call("Runtime.evaluate", {
            "expression": js, "contextId": ctx, "returnByValue": True, "awaitPromise": True,
            "timeout": int(timeout * 1000),
        }, session=self.sid, timeout=timeout + 5)
        if "exceptionDetails" in r:
            raise CdpError(str(r["exceptionDetails"].get("exception", r["exceptionDetails"])))
        return r.get("result", {}).get("value")

    def screenshot(self, path: Path) -> None:
        import base64
        data = self.cdp.call("Page.captureScreenshot", {"format": "png"}, session=self.sid)["data"]
        path.write_bytes(base64.b64decode(data))

    def close(self) -> None:
        self.cdp.close()
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.log.close()
        shutil.rmtree(self.tmp, ignore_errors=True)


def fpc_binary() -> str:
    c = os.environ.get("WSBENCH_FPC")
    if c and Path(c).is_file():
        return c
    raise RuntimeError("fingerprint-chromium not found; set WSBENCH_FPC to its `chrome` binary")


def chrome_version() -> tuple[str, str]:
    out = subprocess.run([chrome_binary(), "--version"], capture_output=True, text=True).stdout
    full = next((w for w in out.split() if w[:1].isdigit()), "0.0.0.0")
    return full.split(".")[0], full


def baseline_fingerprint(seed: str) -> dict[str, Any]:
    """Per-seed values exactly as the baseline derives them (pools, fixed locale)."""
    v = seed_value(seed)
    major, full = chrome_version()
    gpus = [
        ("Google Inc. (Intel)", "ANGLE (Intel, Mesa Intel(R) UHD Graphics 630 (CML GT2), OpenGL 4.6)"),
        ("Google Inc. (Intel)", "ANGLE (Intel, Mesa Intel(R) Iris(R) Xe Graphics (TGL GT2), OpenGL 4.6)"),
        ("Google Inc. (AMD)", "ANGLE (AMD, AMD Radeon RX 6700 XT (navi22, LLVM 17.0.6, DRM 3.54, 6.8.0), OpenGL 4.6)"),
        ("Google Inc. (NVIDIA Corporation)", "ANGLE (NVIDIA Corporation, NVIDIA GeForce RTX 3060/PCIe/SSE2, OpenGL 4.6.0 NVIDIA 550.54.14)"),
    ]
    vendor, renderer = gpus[v % len(gpus)]
    return {
        "seed": v,
        "user_agent": f"Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{full} Safari/537.36",
        "chrome_major": major, "chrome_full_version": full,
        "hardware_concurrency": [8, 12, 16][(v >> 8) % 3],
        "device_memory": [8, 16, 32][(v >> 16) % 3],
        "screen_width": 1920, "screen_height": 1080,
        "webgl_vendor": vendor, "webgl_renderer": renderer,
        # The baseline hard-codes one locale/timezone regardless of egress.
        "languages": ["zh-HK", "zh", "en-US", "en"], "language": "zh-HK",
        "accept_language": "zh-HK,zh;q=0.9,en-US;q=0.8,en;q=0.7",
        "timezone": "Asia/Hong_Kong",
    }


def render_baseline_script(fp: dict[str, Any]) -> str:
    src = (HERE / "baseline_js_hooks.js").read_text()
    subs = {
        "__LANGS__": json.dumps(fp["languages"]), "__LANG__": json.dumps(fp["language"]),
        "__CORES__": str(fp["hardware_concurrency"]), "__MEM__": str(fp["device_memory"]),
        "__VENDOR__": json.dumps(fp["webgl_vendor"]), "__RENDERER__": json.dumps(fp["webgl_renderer"]),
        "__NOISE__": str(fp["seed"] % 9973 + 1),
    }
    for k, v in subs.items():
        src = src.replace(k, v)
    return src


# --------------------------------------------------------------------------- Camoufox


class CamoufoxSession:
    """Headful Camoufox (Firefox + C++ fingerprint spoofing) via Playwright/Juggler.

    Camoufox's generator is not seedable, so a seed maps to a generated identity that
    is persisted on first use (`state_dir/camoufox-<seed>-<egress>.json`) and replayed on
    every later launch, the way a product persists a profile. geoip aligns timezone,
    locale and WebRTC with the egress IP. Page scripts run in Camoufox's isolated world.
    """

    def __init__(self, plan: LaunchPlan, egress: Egress, env: dict[str, str], state_dir: Path):
        from camoufox.fingerprints import Screen
        from camoufox.sync_api import NewBrowser
        from camoufox.utils import launch_options
        from playwright.sync_api import sync_playwright

        self.plan = plan
        cache = state_dir / f"camoufox-{plan.seed}-{egress.name}.json"
        if cache.exists():
            opts = json.loads(cache.read_text())
        else:
            kwargs: dict[str, Any] = dict(
                headless=False, geoip=egress.ip or True, humanize=False,
                # Same egress-derived locale as the Chromium engines; Camoufox's geoip
                # otherwise samples any language spoken in the country (e.g. pa-GB).
                locale=plan.locale,
                env=dict(env), i_know_what_im_doing=True,
                # Keep the generated screen within the Xvfb framebuffer the seed selected.
                screen=Screen(max_width=plan.screen[0], max_height=plan.screen[1]),
            )
            if egress.proxy:
                kwargs["proxy"] = {"server": egress.proxy}
            opts = launch_options(**kwargs)
            cache.parent.mkdir(parents=True, exist_ok=True)
            cache.write_text(json.dumps(opts, default=str))
        # Playwright replaces (not merges) the child environment.
        opts["env"] = {**opts.get("env", {}), **{k: v for k, v in env.items()
                                                 if not k.startswith("CAMOU_CONFIG")}}
        opts["proxy"] = {"server": egress.proxy} if egress.proxy else None
        if opts["proxy"] is None:
            opts.pop("proxy")
        self.config_digest = _camoufox_config_digest(opts)
        self._pw = sync_playwright().start()
        self.browser = NewBrowser(self._pw, from_options=opts)
        self.page = self.browser.new_page()

    def goto(self, url: str, timeout: float = 45.0) -> None:
        try:
            self.page.goto(url, timeout=timeout * 1000, wait_until="load")
        except Exception:
            pass

    def evaluate(self, js: str, timeout: float = 30.0) -> Any:
        return self.page.evaluate(f"async () => {{ return ({js}); }}")

    def screenshot(self, path: Path) -> None:
        self.page.screenshot(path=str(path))

    def close(self) -> None:
        try:
            self.browser.close()
        finally:
            self._pw.stop()


def _camoufox_config_digest(opts: dict[str, Any]) -> str:
    import hashlib
    env = opts.get("env", {})
    blob = "".join(env[k] for k in sorted(env) if k.startswith("CAMOU_CONFIG"))
    return hashlib.sha256(blob.encode()).hexdigest()[:16]


# --------------------------------------------------------------------------- planning

ENGINES = ("fpc", "camoufox", "chrome-seed", "fpc-brand157", "chrome-jshooks")


def plan_launch(engine: str, seed: str, egress: Egress) -> LaunchPlan:
    v = seed_value(seed)
    if engine == "chrome-jshooks":
        fp = baseline_fingerprint(seed)
        return LaunchPlan(engine, seed, (fp["screen_width"], fp["screen_height"]),
                          fp["language"], fp["timezone"], {"fp": fp})
    screen = SCREENS[v % len(SCREENS)]
    locale = COUNTRY_LOCALE.get(egress.country, "en-US")
    extra: dict[str, Any] = {}
    if engine.startswith("fpc"):
        extra["fp_seed"] = v % (2**31 - 1) + 1
    if engine == "chrome-seed":
        pool = [n for n in CORE_POOL if n <= (os.cpu_count() or 4)] or [os.cpu_count() or 1]
        extra["cores"] = pool[(v >> 16) % len(pool)]
    return LaunchPlan(engine, seed, screen, locale, egress.timezone or "UTC", extra)


def open_session(plan: LaunchPlan, egress: Egress, env: dict[str, str], state_dir: Path):
    if plan.engine == "fpc":
        return ChromeSession("fpc", plan, egress, env)
    if plan.engine == "fpc-brand157":
        return ChromeSession("fpc-brand", plan, egress, env)
    if plan.engine == "chrome-seed":
        return ChromeSession("seed", plan, egress, env)
    if plan.engine == "chrome-jshooks":
        return ChromeSession("jshooks", plan, egress, env)
    if plan.engine == "camoufox":
        return CamoufoxSession(plan, egress, env, state_dir)
    raise ValueError(plan.engine)
