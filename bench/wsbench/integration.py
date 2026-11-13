"""Integration capability probe: what a desktop runtime can drive on each engine.

    python -m wsbench.integration

Checks, per engine, over a loopback control channel:
  * protocol         loopback CDP (Chromium) / WebDriver BiDi or Juggler (Firefox)
  * navigate         load a page and read its title
  * a11y             full accessibility tree
  * insert_text      IME-style text insertion into a focused input
  * tabs             open a second tab, list, switch, close
  * profile_carry    open a Chromium user-data-dir written by stock Chrome and read its cookie
"""

from __future__ import annotations

import json
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

from .cdp import Cdp
from .display import WaylandDesktop
from .engines import chrome_binary, fpc_binary

FORM = "data:text/html,<title>wsbench form</title><label for=q>Query</label><input id=q autofocus><button>Go</button>"


def _port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _chromium(binary: str, profile: Path, env: dict, extra: list[str] = ()) -> tuple[subprocess.Popen, Cdp]:
    port = _port()
    p = subprocess.Popen([binary, f"--user-data-dir={profile}", f"--remote-debugging-port={port}",
                          "--no-first-run", "--no-default-browser-check", "--password-store=basic",
                          "--no-sandbox", "--ozone-platform=wayland", *extra, "about:blank"],
                         env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return p, Cdp.from_port(port)


def _stop(p: subprocess.Popen, c: Cdp) -> None:
    try:
        c.call("Browser.close", timeout=10)
    except Exception:
        pass
    c.close()
    try:
        p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill()


def _page_session(c: Cdp) -> str:
    t = next(t for t in c.call("Target.getTargets")["targetInfos"] if t["type"] == "page")
    return c.call("Target.attachToTarget", {"targetId": t["targetId"], "flatten": True})["sessionId"]


def chromium_caps(name: str, binary: str, env: dict) -> dict:
    out: dict = {"engine": name, "protocol": "CDP over loopback WebSocket"}
    tmp = Path(tempfile.mkdtemp(prefix="wsbench-int-"))
    try:
        p, c = _chromium(binary, tmp / "p", env, ["--fingerprint=1234"] if "fpc" in name else [])
        out["version"] = c.call("Browser.getVersion")["product"]
        s = _page_session(c)
        c.call("Page.navigate", {"url": FORM}, session=s)
        time.sleep(1)
        title = c.call("Runtime.evaluate", {"expression": "document.title", "returnByValue": True}, session=s)
        out["navigate"] = title["result"]["value"] == "wsbench form"
        ax = c.call("Accessibility.getFullAXTree", session=s)["nodes"]
        names = {n.get("name", {}).get("value") for n in ax}
        out["a11y"] = "Query" in names and "Go" in names
        c.call("Runtime.evaluate", {"expression": "document.getElementById('q').focus()"}, session=s)
        c.call("Input.insertText", {"text": "héllo 世界"}, session=s)
        v = c.call("Runtime.evaluate", {"expression": "document.getElementById('q').value", "returnByValue": True}, session=s)
        out["insert_text"] = v["result"]["value"] == "héllo 世界"
        t2 = c.call("Target.createTarget", {"url": "about:blank"})["targetId"]
        n = sum(1 for t in c.call("Target.getTargets")["targetInfos"] if t["type"] == "page")
        c.call("Target.activateTarget", {"targetId": t2})
        c.call("Target.closeTarget", {"targetId": t2})
        out["tabs"] = n == 2
        _stop(p, c)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return out


def profile_carry(src_binary: str, dst_binary: str, dst_extra: list[str], env: dict) -> dict:
    """Write a cookie with stock Chrome, reopen the same user-data-dir with `dst_binary`."""
    tmp = Path(tempfile.mkdtemp(prefix="wsbench-carry-"))
    prof = tmp / "p"
    res: dict = {}
    try:
        p, c = _chromium(src_binary, prof, env)
        s = _page_session(c)
        c.call("Page.navigate", {"url": "https://example.com/"}, session=s)
        time.sleep(3)
        c.call("Runtime.evaluate", {"expression":
               "document.cookie = 'wsbench=carry; max-age=86400; path=/; secure'"}, session=s)
        time.sleep(1)
        res["src_version"] = c.call("Browser.getVersion")["product"]
        _stop(p, c)
        time.sleep(1)
        p, c = _chromium(dst_binary, prof, env, dst_extra)
        res["dst_version"] = c.call("Browser.getVersion")["product"]
        cookies = c.call("Storage.getCookies").get("cookies", [])
        res["cookie_carried"] = any(k["name"] == "wsbench" and k["value"] == "carry" for k in cookies)
        res["cookies_seen"] = len(cookies)
        _stop(p, c)
    except Exception as e:
        res["error"] = f"{type(e).__name__}: {e}"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return res


def camoufox_caps(env: dict) -> dict:
    from camoufox.sync_api import Camoufox
    out: dict = {"engine": "camoufox", "protocol": "Juggler over pipe (Playwright)"}
    with Camoufox(headless=False, env=env, i_know_what_im_doing=True) as browser:
        out["version"] = browser.version
        ctx = browser.new_context()
        page = ctx.new_page()
        page.goto(FORM)
        out["navigate"] = page.title() == "wsbench form"
        snap = page.locator("body").aria_snapshot()
        out["a11y"] = "Query" in snap and "Go" in snap
        page.focus("#q")
        page.keyboard.insert_text("héllo 世界")
        out["insert_text"] = page.input_value("#q") == "héllo 世界"
        p2 = ctx.new_page()
        out["tabs"] = len(ctx.pages) == 2
        p2.close()
    out["bidi_loopback"] = camoufox_bidi(env)
    out["profile_carry"] = "no: Firefox profile format; Chromium cookies must be decrypted and re-imported"
    return out


def camoufox_bidi(env: dict) -> str:
    """Does the Camoufox binary serve WebDriver BiDi on --remote-debugging-port?"""
    from camoufox.pkgman import launch_path
    port = _port()
    prof = Path(tempfile.mkdtemp(prefix="wsbench-bidi-"))
    p = subprocess.Popen([launch_path(), "--remote-debugging-port", str(port), "--profile", str(prof),
                          "--no-remote", "about:blank"], env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        from websockets.sync.client import connect
        for _ in range(40):
            try:
                with connect(f"ws://127.0.0.1:{port}/session", open_timeout=2) as ws:
                    ws.send(json.dumps({"id": 1, "method": "session.new", "params": {"capabilities": {}}}))
                    r = json.loads(ws.recv(timeout=10))
                    if r.get("type") == "success":
                        caps = r["result"]["capabilities"]
                        ws.send(json.dumps({"id": 2, "method": "browsingContext.getTree", "params": {}}))
                        tree = json.loads(ws.recv(timeout=10))
                        return (f"yes: WebDriver BiDi session on loopback ({caps.get('browserName')} "
                                f"{caps.get('browserVersion')}), contexts={len(tree.get('result', {}).get('contexts', []))}")
                    return f"BiDi listener refused session: {r.get('error')} {r.get('message', '')[:120]}"
            except (ConnectionRefusedError, OSError, TimeoutError):
                time.sleep(0.5)
        return "no listener on --remote-debugging-port"
    finally:
        p.terminate()
        p.wait(timeout=10)
        shutil.rmtree(prof, ignore_errors=True)


def main() -> None:
    results = []
    with WaylandDesktop(1600, 900) as d:
        env = d.env()
        results.append(chromium_caps("fpc", fpc_binary(), env))
        results.append(chromium_caps("chrome", chrome_binary(), env))
        results[0]["profile_carry"] = profile_carry(chrome_binary(), fpc_binary(), ["--fingerprint=1234"], env)
        try:
            results.append(camoufox_caps(env))
        except Exception as e:
            results.append({"engine": "camoufox", "error": f"{type(e).__name__}: {e}"})
    print(json.dumps(results, indent=2, ensure_ascii=False))


if __name__ == "__main__":
    main()
