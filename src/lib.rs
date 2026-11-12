//! One virtual desktop per agent session.
//!
//! A [`session::Session`] owns one virtual display running one headful
//! browser. Two views act on that same display:
//!
//! - the **browser view** ([`browser`]) drives the browser over the Chrome
//!   DevTools Protocol: navigation, accessibility snapshots with stable
//!   `@snapshot:eN` refs, typing, credential fill, cookies and storage;
//! - the **computer view** ([`computer`]) sends keyboard and pointer input to
//!   the display and captures screenshots of all of it, browser chrome and OS
//!   dialogs included.
//!
//! A person can take over the full display through a noVNC client the crate
//! serves ([`takeover`]); while they hold control every agent call returns
//! [`Error::HumanInControl`] and resumes after they hand back.
//!
//! Policy stays with the caller: secrets come through a resolver trait, the
//! request guard enforces a caller-supplied policy document, the proxy URL and
//! profile directory are caller-owned.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod browser;
pub mod browser_state;
pub mod cdp;
pub mod computer;
pub mod credential;
pub mod display;
pub mod engine;
pub mod environment;
mod error;
pub mod fingerprint;
pub mod profile;
pub mod request_guard;
pub mod session;
pub mod snapshot;
pub mod takeover;
#[cfg(feature = "testkit")]
#[cfg_attr(docsrs, doc(cfg(feature = "testkit")))]
pub mod testkit;

pub use error::{Error, Result};
