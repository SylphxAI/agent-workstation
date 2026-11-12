//! One desktop per agent session.
//!
//! A [`DesktopSession`] is one headless Wayland compositor running exactly
//! one headful browser window with full native chrome. The agent works it
//! two ways that share one state:
//!
//! - **web operations** over the browser's loopback automation endpoint:
//!   navigate, accessibility snapshots with `@eN` refs, click, fill (secrets
//!   resolved by the caller and inserted without ever being returned), tabs;
//! - **desktop operations** through the Wayland output: screenshots of the
//!   whole desktop, pointer and keyboard input, native dialogs included.
//!
//! A person takes over through a full-desktop VNC stream behind a WebSocket
//! and a noVNC page. While they hold control every agent call returns
//! [`Error::HumanInControl`]; the agent resumes when they hand back.
//!
//! Configuration, secrets, profile directories and the proxy URL belong to
//! the caller. There is no global state; HTTP lives only in the takeover page
//! server and the optional daemon binary.

pub mod cdp;
mod error;
pub mod fingerprint;
pub mod gate;
pub mod profile;
pub mod takeover;

pub use error::{Error, Result};
