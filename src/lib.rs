//! One desktop per agent session.
//!
//! A [`DesktopSession`] is one headless Wayland compositor running exactly
//! one headful browser window with full native chrome. The agent works it
//! two ways that share one state:
//!
//! - **web operations** over the browser's loopback DevTools endpoint:
//!   navigate, accessibility snapshots with `@eN` refs bound to their
//!   document, CSS selectors, click, fill (secrets resolved by the caller and
//!   inserted without ever being returned), in-page keys, script evaluation,
//!   HTML, cookies, page screenshots, tabs;
//! - **desktop operations** through the Wayland output: screenshots of the
//!   whole desktop, pointer and keyboard input (held keys and modifiers
//!   included), the clipboard, native dialogs included.
//!
//! One agent principal at a time holds a session ([`DesktopSession::lease`]
//! returns its [`Agent`] view); any other gets [`Error::Leased`]. A person
//! takes over through a full-desktop VNC stream behind a WebSocket and a
//! noVNC page. While they hold control every agent call returns
//! [`Error::HumanInControl`]; the agent resumes when they hand back.
//!
//! Configuration, secrets, profile directories and the proxy URL belong to
//! the caller. There is no global state; HTTP lives only in the takeover page
//! server and the optional daemon binary.

pub mod browser;
pub mod cdp;
#[cfg(target_os = "linux")]
pub mod compositor;
mod desktop;
mod error;
pub mod fingerprint;
pub mod gate;
pub mod profile;
pub mod proxy;
mod secret;
pub mod session;
pub mod takeover;
#[cfg(target_os = "linux")]
pub mod vnc;
#[cfg(target_os = "linux")]
pub mod wayland;

pub use browser::{BrowserConfig, Cookie, Locator, PageImage, Snapshot, Tab};
pub use desktop::{Button, Frame, Input};
pub use error::{Error, Result};
pub use fingerprint::{Fingerprint, FingerprintDefaults};
pub use gate::{ControlGate, HandBack, Holder, Lease};
pub use profile::Profile;
pub use proxy::Proxy;
pub use secret::{Secret, SecretResolver};
pub use session::{Agent, DesktopSession, MAX_HOLD, SessionConfig};
pub use takeover::{Takeover, TakeoverConfig};
