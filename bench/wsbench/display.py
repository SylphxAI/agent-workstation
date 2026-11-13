"""Virtual desktop for one run: Xvfb plus an openbox window manager."""

from __future__ import annotations

import os
import shutil
import subprocess
import time
from pathlib import Path


def _find_free_display(start: int = 140) -> int:
    for n in range(start, start + 400):
        if not Path(f"/tmp/.X11-unix/X{n}").exists() and not Path(f"/tmp/.X{n}-lock").exists():
            return n
    raise RuntimeError("no free X display number")


def _openbox_env(binary: str, display: str) -> dict[str, str]:
    """Environment for openbox; supports an unpacked (non-root) install tree.

    For `<root>/usr/bin/openbox` outside /usr, point the loader and XDG lookups
    at `<root>` so themes, rc.xml and bundled libraries resolve.
    """
    env = {**os.environ, "DISPLAY": display}
    root = Path(binary).resolve().parents[2]
    if root != Path("/") and (root / "etc/xdg/openbox").is_dir():
        env["XDG_CONFIG_DIRS"] = str(root / "etc/xdg")
        env["XDG_DATA_DIRS"] = str(root / "usr/share")
        libdir = root / "usr/lib/x86_64-linux-gnu"
        env["LD_LIBRARY_PATH"] = ":".join(filter(None, [str(libdir), env.get("LD_LIBRARY_PATH")]))
    return env


class Desktop:
    """Starts `Xvfb :N` at the requested resolution and openbox on top of it."""

    def __init__(self, width: int, height: int, depth: int = 24, openbox: str | None = None):
        self.width, self.height, self.depth = width, height, depth
        self.openbox = openbox or os.environ.get("WSBENCH_OPENBOX") or shutil.which("openbox")
        self.display: str | None = None
        self._procs: list[subprocess.Popen] = []

    def __enter__(self) -> "Desktop":
        n = _find_free_display()
        self.display = f":{n}"
        xvfb = subprocess.Popen(
            [
                "Xvfb", self.display,
                "-screen", "0", f"{self.width}x{self.height}x{self.depth}",
                "-ac", "-nolisten", "tcp", "+extension", "GLX", "+extension", "RANDR",
                "+render", "-noreset",
            ],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self._procs.append(xvfb)
        deadline = time.time() + 10
        while not Path(f"/tmp/.X11-unix/X{n}").exists():
            if xvfb.poll() is not None or time.time() > deadline:
                raise RuntimeError(f"Xvfb {self.display} failed to start")
            time.sleep(0.05)
        if not self.openbox:
            raise RuntimeError("openbox not found; set WSBENCH_OPENBOX or install openbox")
        wm = subprocess.Popen(
            [self.openbox, "--sm-disable"],
            env=_openbox_env(self.openbox, self.display),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        self._procs.append(wm)
        time.sleep(0.5)
        if wm.poll() is not None:
            raise RuntimeError("openbox exited immediately")
        return self

    def env(self) -> dict[str, str]:
        assert self.display
        return {**os.environ, "DISPLAY": self.display}

    def __exit__(self, *exc) -> None:
        for p in reversed(self._procs):
            p.terminate()
        for p in reversed(self._procs):
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()


def _unpacked_root_env(binary: str, env: dict[str, str]) -> dict[str, str]:
    """Loader path for a binary unpacked under `<root>/usr/bin` without root."""
    root = Path(binary).resolve().parents[2]
    libdir = root / "usr/lib/x86_64-linux-gnu"
    if root != Path("/") and libdir.is_dir():
        env["LD_LIBRARY_PATH"] = ":".join(filter(None, [str(libdir), env.get("LD_LIBRARY_PATH")]))
    return env


class WaylandDesktop:
    """Headless wlroots compositor (sway, WLR_BACKENDS=headless) with one output.

    Browsers run as native Wayland clients (Chromium --ozone-platform=wayland,
    Firefox MOZ_ENABLE_WAYLAND=1). No X server, no Xwayland.
    """

    def __init__(self, width: int, height: int, sway: str | None = None):
        self.width, self.height = width, height
        self.sway = sway or os.environ.get("WSBENCH_SWAY") or shutil.which("sway")
        self.runtime: Path | None = None
        self.socket: str | None = None
        self._proc: subprocess.Popen | None = None

    def __enter__(self) -> "WaylandDesktop":
        import tempfile
        if not self.sway:
            raise RuntimeError("sway not found; set WSBENCH_SWAY or install sway")
        self.runtime = Path(tempfile.mkdtemp(prefix="wsbench-xdg-"))
        self.runtime.chmod(0o700)
        conf = self.runtime / "sway.conf"
        conf.write_text(
            f"output HEADLESS-1 mode {self.width}x{self.height}\n"
            "xwayland disable\ndefault_border none\nfocus_on_window_activation focus\n"
        )
        env = {k: v for k, v in os.environ.items() if k not in ("DISPLAY", "WAYLAND_DISPLAY")}
        env.update(XDG_RUNTIME_DIR=str(self.runtime), WLR_BACKENDS="headless",
                   WLR_LIBINPUT_NO_DEVICES="1", WLR_RENDERER=os.environ.get("WLR_RENDERER", "pixman"))
        self._proc = subprocess.Popen([self.sway, "-c", str(conf)], env=_unpacked_root_env(self.sway, env),
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.time() + 10
        while True:
            socks = [p.name for p in self.runtime.glob("wayland-*") if not p.name.endswith(".lock")]
            if socks:
                self.socket = socks[0]
                break
            if self._proc.poll() is not None or time.time() > deadline:
                raise RuntimeError("sway failed to start")
            time.sleep(0.05)
        return self

    def env(self) -> dict[str, str]:
        env = {k: v for k, v in os.environ.items() if k != "DISPLAY"}
        env.update(XDG_RUNTIME_DIR=str(self.runtime), WAYLAND_DISPLAY=self.socket or "",
                   XDG_SESSION_TYPE="wayland", MOZ_ENABLE_WAYLAND="1")
        return env

    def __exit__(self, *exc) -> None:
        if self._proc:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self._proc.kill()
        if self.runtime:
            shutil.rmtree(self.runtime, ignore_errors=True)
