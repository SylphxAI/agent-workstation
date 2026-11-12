//! Virtual display backends.
//!
//! A [`DisplayBackend`] starts one [`Display`] per session. The X11 backend
//! ([`Xvfb`]) runs Xvfb with a per-display MIT-MAGIC-COOKIE (no `-ac`, no TCP)
//! and a window manager, so the browser gets real window decorations and OS
//! dialogs render as on a desktop. A Wayland backend (compositor plus VNC)
//! can implement the same traits later.

mod xvfb;

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;

pub use xvfb::{WindowManager, Xvfb};

use crate::error::Result;

/// Starts displays.
#[async_trait]
pub trait DisplayBackend: Send + Sync + fmt::Debug {
    /// Start a display of `width` × `height` pixels.
    async fn start(&self, width: u32, height: u32) -> Result<Box<dyn Display>>;
}

/// A running display.
#[async_trait]
pub trait Display: Send + Sync + fmt::Debug {
    /// Display name, e.g. `:12`.
    fn name(&self) -> &str;

    /// Authority file clients need, when the display uses one.
    fn xauthority(&self) -> Option<&PathBuf>;

    /// Size in pixels.
    fn size(&self) -> (u32, u32);

    /// Environment a client process needs to reach this display.
    fn client_env(&self) -> Vec<(String, String)> {
        let mut env = vec![("DISPLAY".to_string(), self.name().to_string())];
        if let Some(x) = self.xauthority() {
            env.push(("XAUTHORITY".into(), x.display().to_string()));
        }
        env
    }

    /// Whether the display server is still alive.
    fn is_running(&mut self) -> bool;

    /// Stop the window manager and display server.
    async fn shutdown(self: Box<Self>) -> Result<()>;
}
