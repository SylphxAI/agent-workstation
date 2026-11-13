# Changelog

All notable changes to this crate are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (pre-1.0:
a minor version may break the API).

## 0.2.0 (unreleased)

The first release on crates.io. 0.1.0 was consumed by git revision only and
was never published.

### Changed (breaking)

- **One agent principal per session.** Web and desktop operations moved from
  `DesktopSession` to its `Agent` view, which `DesktopSession::lease(principal)`
  returns. Another principal gets `Error::Leased { principal, since }` until
  the holder releases (`Agent::release`, `DesktopSession::release`) or the
  session closes. `ControlGate` gained `acquire`, `release_lease`, `lease`
  and `check(principal)`.
- **`DesktopSession::close(&self)` replaces `shutdown(self)`.** It works
  through a shared reference: new calls fail with `Error::Closed`, calls in
  flight finish, then the browser closes gracefully.
- **Refs are bound to their document.** Any navigation of the document a
  snapshot read (a link, a form, a script, history, reload) makes its refs
  fail with the new `Error::StaleRef`; so does a newer snapshot for a ref
  qualified with an older snapshot's id. `Snapshot` gained `id`, and refs may
  be written `@<snapshot id>:eN`.
- `TakeoverConfig::bind` is an `Option`; `Takeover::local_addr` returns an
  `Option`. The takeover cookie is named per session and scoped to the public
  URL's path.
- Secret fills are bound to the selected node and its owning frame's origin,
  and set the value in an isolated world (untrusted input/change events)
  instead of typing into whatever has focus.

### Added

- Web: CSS selectors wherever a ref is accepted (`Locator`); `focus`;
  in-page keys `press`, `press_and_hold`, `key_down`, `key_up`,
  `release_keys` (held modifiers apply to clicks); `eval` (with
  `Error::Script` for exceptions); `html`; `cookies`, `set_cookies`,
  `delete_cookies`, `clear_cookies` (`Cookie`); `page_screenshot` of the
  viewport or the full page (`PageImage`, cut at `MAX_PAGE_HEIGHT`).
- Desktop: `Input::KeyDown` / `Input::KeyUp` (keys and modifiers held across
  pointer steps, released when the call's steps end), `hold_key`,
  `Button::Back` / `Button::Forward`, `pointer_position` (whichever of the
  agent or the person moved it last), `clipboard_text` /
  `set_clipboard_text` (`wlr-data-control`).
- Takeover: `Takeover::set_public_url` / `DesktopSession::set_takeover_public_url`
  change where people reach the page while the session runs;
  `Takeover::router` / `DesktopSession::takeover_router` mount it in the
  caller's HTTP server (a stopped takeover answers `410 Gone`).
- `ControlGate::hand_backs` counts hand-backs, so an observer that wakes
  after a person took and returned control between two observations still
  sees it; the hand-back note is stored before subscribers are notified.
- Snapshots list out-of-process frames as `Iframe` lines.
- The daemon takes a `principal` on every call, gained the new ops, and
  passes browser flags with `--browser-arg`.

### Fixed

- A desktop Return after a DevTools navigation reloaded the page: keyboard
  focus stayed in the address bar. Navigation, history, reload and tab
  selection now hand native keyboard focus to the page.
- Refs from a previous document could resolve into it (for example through
  the back-forward cache) after a navigation the caller did not make.
- Closing the last tab closed the browser window; a blank tab is opened first.
  `close_tab` returns once the browser has destroyed the tab and refuses an
  unknown tab with `Error::NotFound`.
- Chord parsing treats `shift`, `ctrl`, `alt` and `super` alone as keys.
- Closing a session right after a login crashed Chromium (it segfaults in
  `Browser.close` while "Save password?" is open), losing unwritten cookies.
  Shutdown closes every tab first and keeps `Browser.close` as a fallback.

## 0.1.0

Git-only. Headless-Wayland desktop session with one headful browser, web
operations over loopback DevTools, desktop screenshots and input, and a
full-desktop noVNC takeover.
