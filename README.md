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
  five idle minutes). The page runs on its own listener, can be mounted in
  the caller's HTTP server (`takeover_router`), and its public URL can change
  while the session runs (`set_takeover_public_url`), so a stable public
  entry or a tunnel started later serves it.
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

## Daemon

```sh
AGENT_WORKSTATION_TOKEN=... agent-workstation --state-dir /var/lib/agent-workstation \
  --browser /usr/bin/chromium [--browser-arg FLAG]... --secret-command my-secret-reader --
```

`POST /sessions`, `POST /sessions/{id}/call` with
`{"principal": "agent-a", "op": "navigate", "url": "…"}`, `DELETE
/sessions/{id}`. Ops: `navigate`, `back`, `forward`, `reload`, `current_url`,
`tabs`, `new_tab`, `select_tab`, `close_tab`, `snapshot`, `read`, `click`,
`fill`, `fill_secret`, `focus`, `press` (optional `hold_ms`), `key_down`,
`key_up`, `release_keys`, `eval`, `html`, `cookies`, `set_cookies`,
`delete_cookies`, `clear_cookies`, `page_screenshot`, `screenshot`,
`click_at` (optional `modifiers`), `key`, `hold_key`, `type`, `scroll`,
`pointer_position`, `clipboard_get`, `clipboard_set`, `hand_back`, `release`.
The first principal to call holds the session until it calls `release`;
another principal gets `409` with code `leased`, and a person holding control
answers `409` with code `human-in-control`.

## Runtime requirements (Linux)

The crate compiles on macOS for callers that share configuration and types.
Desktop execution is Linux-only: `DesktopSession::start` returns `Error::Unsupported`
on other platforms before touching the profile or runtime directory. The
compositor, VNC and Wayland backend modules are Linux-only; `Button`, `Frame` and
`Input` remain available at the crate root.

`sway`, `wayvnc` and a Chromium-family browser with Wayland support:

```sh
sudo apt-get install sway wayvnc chromium
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
