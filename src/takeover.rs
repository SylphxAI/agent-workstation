//! Full-desktop takeover: noVNC page and WebSocket bridge to the VNC server.
//! (Placeholder API; the server lands separately.)

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::gate::ControlGate;

/// Takeover server settings.
#[derive(Clone, Debug)]
pub struct TakeoverConfig {
    /// Listen address.
    pub bind: SocketAddr,
    /// Public base URL when behind a reverse proxy.
    pub public_url: Option<url::Url>,
    /// Access token; generated when `None`.
    pub token: Option<String>,
}

impl Default for TakeoverConfig {
    fn default() -> Self {
        Self { bind: ([127, 0, 0, 1], 0).into(), public_url: None, token: None }
    }
}

/// A running takeover server.
#[derive(Debug)]
pub struct Takeover {
    url: url::Url,
    addr: SocketAddr,
}

impl Takeover {
    /// Start serving.
    pub async fn start(_config: TakeoverConfig, _vnc_socket: PathBuf, _gate: Arc<ControlGate>) -> Result<Self> {
        Err(Error::Unsupported("takeover server not built yet".into()))
    }

    /// Page URL including the token.
    pub fn url(&self) -> url::Url {
        self.url.clone()
    }

    /// Bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop serving.
    pub async fn shutdown(self) {}
}
