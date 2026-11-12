# agent-workstation

One desktop per agent session: a headless Wayland compositor running exactly
one headful browser window with full native chrome. The agent works it two
ways that share one state, and a person can take over the whole desktop.

- **Web operations** over the browser's loopback DevTools endpoint: navigate,
  accessibility snapshots with `@eN` refs, click, fill, tabs, page text.
  Secrets are named by a `secret_ref`, resolved by the caller's
  `SecretResolver` at the moment of insertion, inserted with
  `Input.insertText`, and never returned or logged.
- **Desktop operations** through the Wayland output: full-desktop screenshots
  (`wlr-screencopy`), pointer input (`wlr-virtual-pointer`) and keyboard input
  (`virtual-keyboard`, any Unicode text), browser chrome and native dialogs
  included.
- **Takeover**: wayvnc behind a token-protected WebSocket and a noVNC page.
  While a person holds control every agent call returns
  `Error::HumanInControl`; the agent resumes when they hand back (or after
  five idle minutes).
- **No spoofing scripts.** A profile's `fingerprint.json` seed is applied at
  launch only (desktop size, languages, timezone, and an optional seed flag
  for browser builds that take one).
- **Caller-owned everything.** Profile directory, proxy URL (credentials are
  handled by a loopback relay, never put on a command line), secrets,
  runtime directory. No global state; HTTP only in the takeover server and
  the daemon.

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
desk.navigate("https://example.com/").await?;
let snap = desk.snapshot().await?;
println!("{}\ntakeover: {:?}", snap.text, desk.takeover_url());
desk.shutdown().await;
# Ok(()) }
```

## Daemon

```sh
AGENT_WORKSTATION_TOKEN=... agent-workstation --state-dir /var/lib/agent-workstation \
  --browser /usr/bin/chromium --secret-command my-secret-reader --
```

`POST /sessions`, `POST /sessions/{id}/call` with `{"op": "navigate", "url": "…"}`
(ops: `navigate`, `back`, `forward`, `reload`, `current_url`, `tabs`,
`new_tab`, `select_tab`, `close_tab`, `snapshot`, `read`, `click`, `fill`,
`fill_secret`, `screenshot`, `click_at`, `key`, `type`, `scroll`,
`hand_back`), `DELETE /sessions/{id}`. A human holding control answers `409`
with code `human-in-control`.

## Runtime requirements (Linux)

`sway`, `wayvnc` and a Chromium-family browser with Wayland support:

```sh
sudo apt-get install sway wayvnc chromium
```

## Profiles

```text
<dir>/user-data/        browser profile (cookies, storage, sessions)
<dir>/fingerprint.json  seed and launch-time identity, written once, read as-is
```

## License

MIT. The takeover page bundles noVNC (MPL-2.0); see `assets/takeover/novnc/`.
