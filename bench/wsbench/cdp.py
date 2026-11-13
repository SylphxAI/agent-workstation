"""Minimal synchronous Chrome DevTools Protocol client (flattened sessions)."""

from __future__ import annotations

import itertools
import json
import time
import urllib.request
from typing import Any

from websockets.sync.client import connect


class CdpError(RuntimeError):
    pass


class Cdp:
    def __init__(self, ws_url: str):
        self._ws = connect(ws_url, max_size=64 * 1024 * 1024, open_timeout=15,
                           ping_interval=None, max_queue=None)
        self._ids = itertools.count(1)
        self.events: list[dict[str, Any]] = []

    @classmethod
    def from_port(cls, port: int, timeout: float = 20.0) -> "Cdp":
        deadline = time.time() + timeout
        last: Exception | None = None
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=2) as r:
                    return cls(json.load(r)["webSocketDebuggerUrl"])
            except Exception as e:  # browser still starting
                last = e
                time.sleep(0.2)
        raise CdpError(f"DevTools endpoint on port {port} not reachable: {last}")

    def call(self, method: str, params: dict | None = None, session: str | None = None,
             timeout: float = 60.0) -> dict[str, Any]:
        msg_id = next(self._ids)
        msg: dict[str, Any] = {"id": msg_id, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        self._ws.send(json.dumps(msg))
        deadline = time.time() + timeout
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                raise CdpError(f"{method} timed out")
            try:
                data = json.loads(self._ws.recv(timeout=remaining))
            except TimeoutError as e:
                raise CdpError(f"{method} timed out after {timeout:.0f}s") from e
            if data.get("id") == msg_id:
                if "error" in data:
                    raise CdpError(f"{method}: {data['error']}")
                return data.get("result", {})
            if "method" in data:
                self.events.append(data)
                del self.events[:-500]

    def close(self) -> None:
        try:
            self._ws.close()
        except Exception:
            pass
