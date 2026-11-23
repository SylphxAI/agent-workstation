# agent-workstation

One desktop per agent session: a headless Wayland compositor running exactly
one headful browser window with full native chrome. The agent works it two
ways that share one state, one agent owns it at a time, and a person can take
over the whole desktop.

- **Web operations** over the browser's loopback DevTools endpoint: navigate,
  accessibility snapshots with `@eN` refs, CSS selectors, click, fill, keys
  pressed and held inside the page, script evaluation, HTML, cookies, page
  screenshots (viewport or full page), tabs, page text.
  - Refs are bound to the document their snapshot read. Any navigation of
    that document (a link, a form, a script, history, reload) and any newer
    snapshot make them fail with `Error::StaleRef`; `@<snapshot id>:eN`
    names the snapshot explicitly.
  - A DevTools navigation hands native keyboard focus to the page, so desktop
    keys reach the page rather than the address bar. Page keys (`press`,
    `key_down`) reach the page's focused element and never fire browser
    shortcuts.
  - Secrets are named by a `secret_ref`, resolved by the caller's
    `SecretResolver` at the moment of insertion, and never returned or
    logged. Secret fills validate the field's owning-frame origin when an
    expected origin is supplied, and update that exact input/textarea
    synchronously in an isolated world, independent of focus. They emit
    untrusted input/change events, not keyboard events; ordinary fills
    continue to use trusted browser input. Detached or replaced nodes fail
    closed.
  - Snapshots include in-process child frames. Out-of-process (cross-site)
    frames appear as an `Iframe` line without their content; reach them
    through desktop operations.
- **Desktop operations** through the Wayland output: full-desktop screenshots
  (`wlr-screencopy`), pointer input (`wlr-virtual-pointer`) including the back
  and forward buttons, keyboard input (`virtual-keyboard`, any Unicode text)
  with keys and modifiers held across pointer steps, the pointer position, and
  the clipboard (`wlr-data-control`). Browser chrome and native dialogs are
  included.
- **One agent at a time.** `DesktopSession::lease(principal)` gives that
  principal its `Agent` view, through which every operation goes. Another
  principal gets `Error::Leased`, naming the holder, until the holder releases
  the lease or the session closes.
- **Takeover**: wayvnc behind a token-protected WebSocket and a noVNC page.
  While a person holds control every agent call returns
  `Error::HumanInControl`; the agent resumes when they hand back (or after
  the idle timeout expires). The page runs on its own listener, can be mounted
  in the caller's HTTP server (`takeover_router`), and its public URL can change
  while the session runs (`set_takeover_public_url`), so a stable public
  entry or a tunnel started later serves it.
  - Human idle expiry is configurable through
    `SessionConfig::human_idle_timeout` (default
    `Some(Duration::from_secs(300))`). Set `None` to require an explicit
    hand-back, or use a longer duration; takeover state reports nullable
    `idle_timeout_ms`/`idle_remaining_ms` for indefinite control.
  - Click the desktop to use keyboard shortcuts. On macOS, common `Cmd`
    shortcuts map to Linux `Ctrl` shortcuts, `Cmd+←/→` moves to the start/end
    of the line, and `Option+←/→` moves by word. Windows and Linux keep `Ctrl`.
    Some shortcuts may be reserved by the host browser or operating system.
  - While you control a Wayland session, paste UTF-8 text directly into the
    desktop. Copying or cutting in the desktop also updates your local clipboard
    when the page has focus and the browser allows it. If blocked, use the retry
    button or open **Clipboard** for **Copy from desktop** and **Paste to desktop**. Automatic browser clipboard access may
    require HTTPS (or localhost) and browser permission. Clipboard sync is off
    in view-only mode.
  - On a phone, use **Trackpad** to aim with one finger or switch to **Direct
    Touch** to click where you tap. Hold for right-click, pinch to zoom, or
    slide two fingers to scroll. Turn on **Pan** in View controls to move the
    camera instead. The **Keyboard** and **Keys** buttons stay reachable
    above the on-screen keyboard; selected modifiers apply to the next key.
  - The status strip shows who holds control and the HTTP round-trip time to
    the desktop gateway (not video latency). **Reconnect** replaces the viewer
    connection without restarting the desktop or changing who has control.
  - If the browser exits, the takeover page reports it without releasing
    control or restarting automatically, showing holder-specific guidance to
    resume with the existing profile.
- **No spoofing scripts.** A profile's `fingerprint.json` seed is applied at
  launch only (desktop size, languages, timezone, and an optional seed flag
  for browser builds that take one).
- **Caller-owned everything.** Profile directory, proxy URL (credentials are
  handled by a loopback relay, never put on a command line), secrets,
  runtime directory, principals. No global state; HTTP only in the takeover
  server and the daemon.

## Library

```rust,no_run
use agent_workstation::{BrowserConfig, DesktopSession, Profile, SessionConfig};

# async fn run() -> agent_workstation::Result<()> {
let cfg = SessionConfig::new(
    Profile::new("work", "/var/lib/profiles/work"),
    BrowserConfig::new("/usr/bin/chromium"),
    "/run/user/1000/desk-1",
);
let desk = DesktopSession::start(cfg).await?;
let agent = desk.lease("agent-42")?;
agent.navigate("https://example.com/").await?;
let snap = agent.snapshot().await?;
println!("{}\ntakeover: {:?}", snap.text, desk.takeover_url());
desk.close().await;
# Ok(()) }
```

`DesktopSession::start` returns only after the browser's native Wayland window
is mapped and owns keyboard focus. Seat input devices are registered before
the browser starts; a DevTools connection alone is not treated as desktop
readiness. Missing native focus fails startup with a bounded timeout.

`DesktopSession::close` works from a shared reference: new calls fail with
`Error::Closed`, calls in flight finish, then the browser closes gracefully
(flushing the profile) and the other processes stop.

Native input completion acknowledges compositor processing, not application
handling. In particular, `key("ctrl+l")` does not mean the browser has finished
moving focus to its address bar: observe the resulting UI state before typing
text that depends on it. The generic input layer does not recognize browser
shortcuts or resend input. Keys held by `Input::KeyDown` are released when
the steps of that `input` call end.

## Daemon & Compact CLI

```sh
AGENT_WORKSTATION_TOKEN=... agent-workstation --state-dir /var/lib/agent-workstation \
  --browser /usr/bin/chromium [--browser-arg FLAG]... --secret-command my-secret-reader --
```

Compact CLI subcommands drive a running daemon using `AGENT_WORKSTATION_URL`
(default `http://127.0.0.1:7600`) and `AGENT_WORKSTATION_TOKEN`:

```sh
agent-workstation open https://example.com
agent-workstation snapshot
agent-workstation click @e1
agent-workstation fill @e2 "person@example.com"
agent-workstation fill-secret @e3 vault/item/password --expect-origin https://example.com
agent-workstation eval "document.title"
agent-workstation screenshot ./page.png
agent-workstation desktop snapshot
agent-workstation desktop focus '@<snapshot>:e1'   # use the exact ref printed above
agent-workstation takeover start
agent-workstation close
```

On Linux, `desktop snapshot` uses the session's private AT-SPI2 accessibility
bus first. Run `agent-workstation --json desktop snapshot` to see the mode:

- `native_accessibility`: use each node's `qualified_ref`. `desktop click` invokes
  a control; `desktop focus` focuses it natively; `desktop set-value` replaces
  text on controls that implement AT-SPI EditableText. For a control that supports
  focus but not EditableText, focus it by ref, then use `desktop type`.
  Pointer gestures (double-click, other buttons, modifiers) require explicit
  coordinates or a compositor window ref, not a semantic ref.
- `compositor`: the response includes a `fallback_reason`. Refs name mapped
  windows only; clicking one targets the window center, never a control.

Session refs include a session and snapshot generation. Copy the full ref and
use it before taking another snapshot. If `truncated` is true, re-root the read
with `desktop snapshot --root '<qualified-ref>'`; use `--offset` to skip
immediate children at that root (or top-level entries without `--root`).
A subtree snapshot also rotates refs: use the newly returned root ref to page
further. `desktop --host` still targets the local OS, not the daemon session.

Version-synced progressive-disclosure agent instructions are built into the binary
and work offline:

```sh
agent-workstation skills            # compact topic index + exact binary version
agent-workstation skills core       # browser workflow & secret hygiene
agent-workstation skills desktop    # Wayland desktop window & input controls
agent-workstation skills takeover   # human noVNC handoff & atomic close
```

`POST /sessions`, `POST /sessions/{id}/call` with
`{"principal": "agent-a", "op": "navigate", "url": "…"}`, `GET /sessions/{id}/takeover`,
`POST /sessions/{id}/takeover/{start,end}`, `DELETE /sessions/{id}[?if_agent=true]`. Ops: `navigate`, `back`, `forward`, `reload`, `current_url`,
`tabs`, `new_tab`, `select_tab`, `close_tab`, `snapshot`, `read`, `click`,
`fill`, `fill_secret`, `focus`, `press` (optional `hold_ms`), `key_down`,
`key_up`, `release_keys`, `eval`, `html`, `cookies`, `set_cookies`,
`delete_cookies`, `clear_cookies`, `page_screenshot`, `screenshot`,
`click_at` (optional `modifiers`), `desktop_windows`, `desktop_snapshot`,
`desktop_focus`, `desktop_click_ref`, `key`, `hold_key`, `type`, `scroll`,
`pointer_position`, `clipboard_get`, `clipboard_set`, `hand_back`, `release`.
The first principal to call holds the session until it calls `release`;
another principal gets `409` with code `leased`, and a person holding control
answers `409` with code `human-in-control`.

### Supervision and snapshot admission

Every daemon route requires `Authorization: Bearer <token>`, using the
16-or-more-character `AGENT_WORKSTATION_TOKEN` supplied at startup. Missing
or invalid credentials return `401` before request-body validation.

| Route | Response |
| --- | --- |
| `GET /healthz` | `{"status":"ok","sessions":0,"quiesced":false}` |
| `GET /sessions` | `{"sessions":[{"id":"…","takeover_url":"…","quiesced":false,"in_flight":0}]}` |
| `POST /quiesce`, `POST /sessions/{id}/quiesce` | Fence admission, then wait for in-flight work to drain |
| `POST /resume`, `POST /sessions/{id}/resume` | Clear that scope's fence; `{"quiesced":false}` |

Quiesce accepts an empty body, `{}`, or `{"timeout_ms":250}`. The default
budget is 250 ms, clamped to 1–250 ms. Success returns
`{"quiesced":true,"drained_ms":0}`. If work remains at the deadline, it
returns `503 Service Unavailable` with `{"code":"quiesce_timeout","message":"in-flight calls did not drain within budget"}`.
**A timeout leaves the fence set**: retry quiesce to verify drain, or resume
to restore admission. Do not snapshot after a timeout.

While fenced, new calls (and creation under the daemon-wide fence) return
`409` with `{"code":"quiesced","message":"session is quiesced for snapshot"}`.
Counters include admitted requests waiting for their bodies and are released
even on validation errors, cancellation, or unwinding. Daemon-wide drain also
covers session creation and deletion; per-session counters include deletion
so lifecycle changes cannot race a successful drain. Reads and resume remain
available. Unknown session quiesce/resume returns `404`.

Resume accepts an empty body or `{}`. A global resume does not clear local
session fences, and a local resume does not clear the global fence. The list
reports the effective (global OR local) fence. A concurrent resume interrupts
a waiting quiesce with `409` / `quiesce_cancelled`; serialize snapshot
ownership in the caller. Session IDs are sorted; a disabled takeover endpoint
is represented by an empty URL string.

These hooks drain **daemon HTTP operations**, not autonomous browser writes,
background tasks, or filesystem buffers. A snapshot controller must arrange
VM/filesystem consistency separately, retain the fence until snapshot capture
finishes, and resume afterward (including recovery paths). Each control handler
starts its deadline before reading the body (limited to 64 KiB). Quiesce shares
one deadline across body reading and drain, capped at 250 ms from handler entry;
resume body reading has the same cap. A stalled control body returns
`408` / `control_body_timeout` without changing fences. The 250 ms server-side
budget leaves margin below 300 ms; it is not an end-to-end network, scheduler,
filesystem, or VM snapshot latency guarantee.

## Runtime requirements & Native Host `PlatformAdapter`

The crate compiles on Linux, macOS, and Windows:

- **Headless Wayland browser sessions (`DesktopSession`)**: Linux-only (`sway`,
  `wayvnc`, and a Chromium-family browser). On macOS and Windows,
  `DesktopSession::start` returns `Error::Unsupported` before touching the
  profile or runtime directory.
- **Native host desktop automation (`platform::PlatformAdapter`)**: Pure-Rust
  compile-time accessibility tree and input adapters (`native_adapter()`, also
  wired to `agent-workstation desktop --host ...`) returning structured
  `DesktopTree` JSON with deterministic `@<snap>:eN` refs and `StaleRef`
  invalidation:
  - **Linux**: AT-SPI2 (D-Bus) + Wayland/`sway` compositor integration.
  - **macOS**: Accessibility (`AXUIElement`) + CoreFoundation + CoreGraphics FFI.
  - **Windows**: UI Automation (`IUIAutomation` COM) + Win32 input.

For headless Linux sessions, install `sway`, `wayvnc`, and a Chromium-family
browser with Wayland support:

```sh
sudo apt-get install sway wayvnc chromium dbus at-spi2-core
```

## Profiles

```text
<dir>/user-data/        browser profile (cookies, storage, sessions)
<dir>/fingerprint.json  seed and launch-time identity, written once, read as-is
```

## Releases

Releases are manual: record the version's changes in `CHANGELOG.md`, then
dispatch the `release` workflow on `main` and supply the exact version in
`Cargo.toml`. Main pushes and tags never publish automatically.
The workflow skips versions already on crates.io, rejects unexpected registry
responses and existing tags for unpublished versions, publishes with a
short-lived OIDC token, then creates the version tag. No static registry secret
is used. If publishing succeeds but tagging fails, verify the published source
before repairing the missing tag; redispatching skips an already published
version rather than tagging a potentially different commit.

Before dispatch, configure the GitHub environment `crates-io` to allow releases
from `main`, and register the corresponding crates.io trusted publisher for
this repository, workflow filename `release.yml`, and environment `crates-io`.
Follow the [crates.io trusted publishing documentation](https://crates.io/docs/trusted-publishing)
for first-publication setup; workflow authentication does not itself register a
publisher or grant ownership of a crate name. Keep publisher registration and
publication as separate deliberate operations.

## License

MIT. The takeover page bundles noVNC (MPL-2.0); see `assets/takeover/novnc/`.
