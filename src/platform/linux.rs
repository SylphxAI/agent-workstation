//! Linux native host desktop backend: AT-SPI2 (D-Bus) + Wayland/`sway` compositor.
//!
//! Pure safe Rust (`#![forbid(unsafe_code)]` clean): implements a minimal
//! little-endian D-Bus wire client over Unix sockets for AT-SPI2 accessibility
//! tree queries and actions, combined with the crate's existing Wayland input
//! (`wayland::InputDevice`) and `sway` IPC (`compositor`) integrations.

use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::compositor;
use crate::desktop::{Button, DesktopFallbackReason, DesktopRect, DesktopWindow, Input};
use crate::platform::{
    AdapterError, DesktopAction, DesktopTree, NodeRef, PlatformAdapter, PlatformCapabilities,
    RawNode, SnapshotIndex, TreeOptions, parse_chord,
};
use crate::wayland::InputDevice;

const DBUS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DBUS_BODY: usize = 8 * 1024 * 1024;

// AT-SPI2 state bit indices (org.a11y.atspi.StateType):
// state_set is `au` (array of 32-bit bitmasks); index = bit / 32, mask = 1 << (bit % 32).
const ATSPI_STATE_ENABLED: u32 = 8;
const ATSPI_STATE_FOCUSED: u32 = 12;

/// Handle stored in [`SnapshotIndex`] for each Linux accessibility node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinuxHandle {
    /// An AT-SPI2 accessible object identified by D-Bus bus name and object path.
    AtSpi {
        /// Owning D-Bus unique or well-known bus name.
        bus: String,
        /// Accessible object path (e.g. `/org/a11y/atspi/accessible/42`).
        path: String,
    },
    /// A Wayland/`sway` compositor container id (`con_id`).
    SwayWindow(u64),
}

/// Compile-time Linux [`PlatformAdapter`] combining AT-SPI2 and Wayland/`sway`.
#[derive(Debug)]
pub struct LinuxPlatformAdapter {
    atspi_socket: Option<PathBuf>,
    session: Option<SessionAccessibility>,
    sway_socket: Option<PathBuf>,
    wayland_socket: Option<PathBuf>,
    wayland_size: (u32, u32),
    index: Mutex<SnapshotIndex<LinuxHandle>>,
}

#[derive(Debug)]
struct SessionAccessibility {
    bus: Option<PathBuf>,
    // Unique names are only unique within a single bus lifetime.
    guid: Mutex<Option<String>>,
}

pub(crate) enum SessionCapture {
    Native(DesktopTree),
    Degraded(DesktopFallbackReason),
}

impl Default for LinuxPlatformAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxPlatformAdapter {
    /// Discover host AT-SPI2, `sway`, and Wayland sockets from the process environment.
    pub fn new() -> Self {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let atspi_socket = discover_atspi_socket_sync(runtime_dir.as_deref());
        let sway_socket = std::env::var_os("SWAYSOCK")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let wayland_socket = std::env::var_os("WAYLAND_DISPLAY")
            .filter(|v| !v.is_empty())
            .and_then(|display| {
                let p = PathBuf::from(&display);
                if p.is_absolute() {
                    Some(p)
                } else {
                    runtime_dir.as_ref().map(|rt| rt.join(p))
                }
            });
        Self {
            atspi_socket,
            session: None,
            sway_socket,
            wayland_socket,
            wayland_size: (1920, 1080),
            index: Mutex::new(SnapshotIndex::default()),
        }
    }

    /// Construct an adapter with explicit socket paths (for sessions or isolated tests).
    pub fn with_sockets(
        atspi_socket: Option<PathBuf>,
        sway_socket: Option<PathBuf>,
        wayland_socket: Option<PathBuf>,
        wayland_size: (u32, u32),
    ) -> Self {
        Self {
            atspi_socket,
            session: None,
            sway_socket,
            wayland_socket,
            wayland_size,
            index: Mutex::new(SnapshotIndex::default()),
        }
    }

    pub(crate) fn for_session(session_bus: Option<PathBuf>, scope: String) -> Self {
        Self {
            atspi_socket: None,
            session: Some(SessionAccessibility {
                bus: session_bus,
                guid: Mutex::new(None),
            }),
            sway_socket: None,
            wayland_socket: None,
            wayland_size: (0, 0),
            index: Mutex::new(SnapshotIndex::scoped(scope)),
        }
    }

    pub(crate) fn clear(&self) {
        self.index.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    pub(crate) fn validate_target(&self, target: &NodeRef) -> Result<(), AdapterError> {
        self.index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resolve(target)?;
        Ok(())
    }

    pub(crate) async fn capture_session(
        &self,
        options: TreeOptions,
        root: Option<NodeRef>,
        offset: usize,
    ) -> Result<SessionCapture, crate::Error> {
        let session = self
            .session
            .as_ref()
            .expect("session accessibility adapter");
        let selected = if let Some(target) = &root {
            let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            let (node, handle) = index.resolve(target)?;
            Some((node.app_id.clone(), node.pid, handle.clone()))
        } else {
            None
        };
        self.clear();
        let (depth, nodes) = options.effective_bounds();
        let capture = async {
            let mut conn = if let Some(target) = &root {
                self.connect_for_action(target).await?
            } else {
                self.connect_atspi().await?
            };
            let mut walk = Walk::new(depth, nodes, true);
            let roots = if let Some((app_id, pid, LinuxHandle::AtSpi { bus, path })) = selected {
                walk_atspi_node(&mut conn, &bus, &path, app_id, pid, 1, offset, &mut walk)
                    .await
                    .into_iter()
                    .collect()
            } else {
                walk_atspi_apps(&mut conn, options.pid, offset, &mut walk).await?
            };
            Ok::<_, AdapterError>((roots, walk.truncated, conn.guid))
        };
        let (roots, truncated, guid) = match tokio::time::timeout(DBUS_TIMEOUT, capture).await {
            Ok(Ok(result)) => result,
            Ok(Err(err)) if root.is_some() => return Err(err.into()),
            Ok(Err(_)) => {
                return Ok(SessionCapture::Degraded(
                    DesktopFallbackReason::AccessibilityUnavailable,
                ));
            }
            Err(_) if root.is_some() => {
                return Err(crate::Error::Timeout("accessibility subtree".into()));
            }
            Err(_) => {
                return Ok(SessionCapture::Degraded(
                    DesktopFallbackReason::AccessibilityTimeout,
                ));
            }
        };
        if roots.is_empty() && !truncated && root.is_none() && options.pid.is_none() && offset == 0
        {
            return Ok(SessionCapture::Degraded(
                DesktopFallbackReason::NoAccessibleApplications,
            ));
        }
        if roots.is_empty() && root.is_some() {
            return Err(crate::Error::StaleRef(
                "accessibility subtree no longer exists".into(),
            ));
        }
        *session.guid.lock().unwrap_or_else(|p| p.into_inner()) = Some(guid);
        let mut tree = self
            .index
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .build("linux", roots, options);
        tree.truncated |= truncated;
        if truncated {
            tree.text = tree.render_outline();
        }
        Ok(SessionCapture::Native(tree))
    }

    async fn connect_atspi(&self) -> Result<DbusConn, AdapterError> {
        let path = self
            .resolve_atspi_socket()
            .await?
            .ok_or_else(|| AdapterError::Backend("AT-SPI2 bus is not reachable".into()))?;
        DbusConn::connect(&path).await
    }

    async fn connect_for_action(&self, target: &NodeRef) -> Result<DbusConn, AdapterError> {
        let conn = self.connect_atspi().await?;
        if let Some(session) = &self.session
            && session
                .guid
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_deref()
                != Some(&conn.guid)
        {
            return Err(AdapterError::StaleRef(format!(
                "{target}: accessibility bus was replaced"
            )));
        }
        Ok(conn)
    }

    async fn resolve_atspi_socket(&self) -> Result<Option<PathBuf>, AdapterError> {
        if let Some(path) = &self.atspi_socket {
            return Ok(Some(path.clone()));
        }
        // Query session D-Bus `org.a11y.Bus.GetAddress` when present.
        let session_bus = match &self.session {
            Some(session) => session.bus.clone(),
            None => discover_session_bus_socket(),
        };
        if let Some(session_bus) = session_bus {
            match DbusConn::connect(&session_bus).await {
                Ok(mut conn) => {
                    if let Ok(addr) = conn
                        .call_string(
                            "org.a11y.Bus",
                            "/org/a11y/bus",
                            "org.a11y.Bus",
                            "GetAddress",
                            "",
                            &[],
                        )
                        .await
                        && let Some(path) = parse_unix_dbus_path(&addr)
                    {
                        return Ok(Some(path));
                    }
                }
                Err(AdapterError::PermissionDenied(m)) => {
                    return Err(AdapterError::PermissionDenied(m));
                }
                Err(_) => {}
            }
        }
        Ok(None)
    }

    async fn run_wayland_steps(&self, steps: Vec<Input>) -> Result<(), AdapterError> {
        let socket = self.wayland_socket.clone().ok_or_else(|| {
            AdapterError::UnsupportedAction(
                "Wayland input unavailable (WAYLAND_DISPLAY is not configured)".into(),
            )
        })?;
        let size = self.wayland_size;
        tokio::task::spawn_blocking(move || {
            let mut dev = InputDevice::open(&socket, size)
                .map_err(|e| AdapterError::Backend(format!("wayland input open: {e}")))?;
            dev.run(&steps)
                .map_err(|e| AdapterError::Backend(format!("wayland input dispatch: {e}")))
        })
        .await
        .map_err(|e| AdapterError::Backend(format!("wayland worker join: {e}")))?
    }
}

#[async_trait]
impl PlatformAdapter for LinuxPlatformAdapter {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            platform: "linux".into(),
            backend: "linux-atspi2+wayland".into(),
            accessibility_tree: self.atspi_socket.is_some()
                || discover_session_bus_socket().is_some()
                || self.sway_socket.is_some(),
            native_actions: self.atspi_socket.is_some() || self.sway_socket.is_some(),
            keyboard_injection: self.wayland_socket.is_some(),
            pointer_injection: self.wayland_socket.is_some(),
            window_enumeration: self.atspi_socket.is_some() || self.sway_socket.is_some(),
        }
    }

    async fn snapshot(&self, options: TreeOptions) -> Result<DesktopTree, AdapterError> {
        let (max_depth, max_nodes) = options.effective_bounds();

        // 1. Prefer AT-SPI2 accessibility tree when reachable.
        if let Some(bus_path) = self.resolve_atspi_socket().await? {
            let mut conn = DbusConn::connect(&bus_path).await?;
            let roots = walk_atspi_root(&mut conn, max_depth, max_nodes, options.pid).await?;
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            return Ok(index.build("linux", roots, options));
        }

        // 2. Fall back to sway/Wayland compositor window tree when available.
        if let Some(sway_sock) = &self.sway_socket {
            let windows = compositor::list_windows(sway_sock)
                .await
                .map_err(map_crate_error)?;
            let raw_roots: Vec<RawNode<LinuxHandle>> = windows
                .into_iter()
                .filter(|w| options.pid.is_none_or(|pid| w.pid == Some(pid)))
                .map(|w| RawNode {
                    handle: LinuxHandle::SwayWindow(w.id),
                    role: "window".into(),
                    name: w.name,
                    value: None,
                    app_id: w.app_id,
                    pid: w.pid,
                    enabled: true,
                    focused: w.focused,
                    bounds: Some(w.rect),
                    actions: vec!["focus".into(), "click".into()],
                    children: Vec::new(),
                })
                .collect();
            let mut index = self.index.lock().unwrap_or_else(|p| p.into_inner());
            return Ok(index.build("linux", raw_roots, options));
        }

        Err(AdapterError::Backend(
            "no Linux accessibility bus (AT-SPI2) or sway compositor socket found".into(),
        ))
    }

    async fn act(&self, action: DesktopAction) -> Result<(), AdapterError> {
        match action {
            DesktopAction::Click { target, button } => {
                let (node, handle) = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    let (n, h) = idx.resolve(&target)?;
                    (n.clone(), h.clone())
                };
                match handle {
                    LinuxHandle::AtSpi { bus, path }
                        if button.unwrap_or(Button::Left) == Button::Left =>
                    {
                        let mut conn = self.connect_for_action(&target).await?;
                        let mut body = Vec::new();
                        encode_i32(&mut body, 0);
                        let ok = conn
                            .call_bool(&bus, &path, "org.a11y.atspi.Action", "DoAction", "i", &body)
                            .await;
                        if self.session.is_some() {
                            return native_action_result(ok, &target, "DoAction(0)");
                        }
                        if ok.unwrap_or(false) {
                            return Ok(());
                        }
                        if let Some(bounds) = node.bounds {
                            let (x, y) = bounds.center();
                            return self
                                .run_wayland_steps(click_steps(x, y, Button::Left))
                                .await;
                        }
                        Err(AdapterError::UnsupportedAction(format!(
                            "AT-SPI2 node {target} rejected DoAction(0) and has no bounds"
                        )))
                    }
                    LinuxHandle::AtSpi { .. } | LinuxHandle::SwayWindow(_) => {
                        let bounds = node.bounds.ok_or_else(|| {
                            AdapterError::UnsupportedAction(format!(
                                "node {target} has no screen bounds for pointer click"
                            ))
                        })?;
                        let (x, y) = bounds.center();
                        self.run_wayland_steps(click_steps(x, y, button.unwrap_or(Button::Left)))
                            .await
                    }
                }
            }
            DesktopAction::DoubleClick { target } => {
                let bounds = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    let (n, _) = idx.resolve(&target)?;
                    n.bounds.ok_or_else(|| {
                        AdapterError::UnsupportedAction(format!(
                            "node {target} has no screen bounds for double-click"
                        ))
                    })?
                };
                let (x, y) = bounds.center();
                let mut steps = click_steps(x, y, Button::Left);
                steps.push(Input::Wait(Duration::from_millis(40)));
                steps.extend(click_steps(x, y, Button::Left));
                self.run_wayland_steps(steps).await
            }
            DesktopAction::Focus { target } => {
                let handle = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    idx.resolve(&target)?.1.clone()
                };
                match handle {
                    LinuxHandle::AtSpi { bus, path } => {
                        let mut conn = self.connect_for_action(&target).await?;
                        let ok = conn
                            .call_bool(
                                &bus,
                                &path,
                                "org.a11y.atspi.Component",
                                "GrabFocus",
                                "",
                                &[],
                            )
                            .await;
                        if self.session.is_some() {
                            return native_action_result(ok, &target, "GrabFocus");
                        }
                        if ok? {
                            Ok(())
                        } else {
                            Err(AdapterError::UnsupportedAction(format!(
                                "AT-SPI2 node {target} refused GrabFocus"
                            )))
                        }
                    }
                    LinuxHandle::SwayWindow(id) => {
                        let sock = self.sway_socket.as_ref().ok_or_else(|| {
                            AdapterError::Backend("sway IPC socket unavailable".into())
                        })?;
                        compositor::focus_window(sock, id)
                            .await
                            .map_err(map_crate_error)
                    }
                }
            }
            DesktopAction::SetValue { target, value } => {
                let handle = {
                    let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                    idx.resolve(&target)?.1.clone()
                };
                match handle {
                    LinuxHandle::AtSpi { bus, path } => {
                        let mut conn = self.connect_for_action(&target).await?;
                        let mut body = Vec::new();
                        encode_string(&mut body, &value);
                        let ok = conn
                            .call_bool(
                                &bus,
                                &path,
                                "org.a11y.atspi.EditableText",
                                "SetTextContents",
                                "s",
                                &body,
                            )
                            .await;
                        if self.session.is_some() {
                            return native_action_result(ok, &target, "SetTextContents");
                        }
                        if ok? {
                            Ok(())
                        } else {
                            Err(AdapterError::UnsupportedAction(format!(
                                "AT-SPI2 node {target} refused SetTextContents"
                            )))
                        }
                    }
                    LinuxHandle::SwayWindow(_) => Err(AdapterError::UnsupportedAction(
                        "SetValue requires an AT-SPI2 editable control".into(),
                    )),
                }
            }
            DesktopAction::TypeText { text } => {
                self.run_wayland_steps(vec![Input::Text(text)]).await
            }
            DesktopAction::PressKey { chord } => {
                parse_chord(&chord)?;
                self.run_wayland_steps(vec![Input::Key(chord)]).await
            }
            DesktopAction::Scroll { target, dx, dy } => {
                let mut steps = Vec::new();
                if let Some(target) = target {
                    let bounds = {
                        let idx = self.index.lock().unwrap_or_else(|p| p.into_inner());
                        let (n, _) = idx.resolve(&target)?;
                        n.bounds
                    };
                    if let Some(b) = bounds {
                        let (x, y) = b.center();
                        steps.push(Input::Move { x, y });
                    }
                }
                steps.push(Input::Scroll { dx, dy });
                self.run_wayland_steps(steps).await
            }
        }
    }

    async fn list_windows(&self) -> Result<Vec<DesktopWindow>, AdapterError> {
        if let Some(sway_sock) = &self.sway_socket {
            return compositor::list_windows(sway_sock)
                .await
                .map_err(map_crate_error);
        }
        if let Some(bus_path) = self.resolve_atspi_socket().await? {
            let mut conn = DbusConn::connect(&bus_path).await?;
            let roots = walk_atspi_root(&mut conn, 2, 200, None).await?;
            let mut windows = Vec::new();
            let mut id = 1u64;
            for app in roots {
                for child in app.children {
                    windows.push(DesktopWindow {
                        id,
                        name: child.name,
                        app_id: child.app_id.or(app.app_id.clone()),
                        pid: child.pid.or(app.pid),
                        focused: child.focused,
                        rect: child.bounds.unwrap_or_default(),
                    });
                    id += 1;
                }
            }
            return Ok(windows);
        }
        Err(AdapterError::Backend(
            "no Linux accessibility bus (AT-SPI2) or sway compositor socket found".into(),
        ))
    }
}

fn native_action_result(
    result: Result<bool, AdapterError>,
    target: &NodeRef,
    method: &str,
) -> Result<(), AdapterError> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => Err(AdapterError::UnsupportedAction(format!(
            "{target} refused {method}"
        ))),
        Err(err) => Err(err),
    }
}

fn click_steps(x: f64, y: f64, button: Button) -> Vec<Input> {
    vec![
        Input::Move { x, y },
        Input::Button {
            button,
            pressed: true,
        },
        Input::Button {
            button,
            pressed: false,
        },
    ]
}

fn map_crate_error(err: crate::Error) -> AdapterError {
    match err {
        crate::Error::Io(io) if io.kind() == ErrorKind::PermissionDenied => {
            AdapterError::PermissionDenied(io.to_string())
        }
        crate::Error::NotFound(m) => AdapterError::NodeNotFound(m),
        crate::Error::InvalidInput(m) => AdapterError::InvalidInput(m),
        crate::Error::Unsupported(m) => AdapterError::UnsupportedPlatform(m),
        crate::Error::Refused(m) => AdapterError::PermissionDenied(m),
        other => AdapterError::Backend(other.to_string()),
    }
}

fn discover_atspi_socket_sync(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    if let Ok(addr) = std::env::var("AT_SPI_BUS_ADDRESS")
        && let Some(path) = parse_unix_dbus_path(&addr)
    {
        return Some(path);
    }
    if let Some(rt) = runtime_dir {
        for candidate in ["at-spi/bus_0", "at-spi/bus"] {
            let p = rt.join(candidate);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn discover_session_bus_socket() -> Option<PathBuf> {
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        && let Some(path) = parse_unix_dbus_path(&addr)
    {
        return Some(path);
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|rt| PathBuf::from(rt).join("bus"))
        .filter(|p| p.exists())
}

/// Extract a filesystem or Linux abstract Unix socket path from a D-Bus address such as
/// `unix:path=/run/user/1000/at-spi/bus_0,guid=...`.
pub(crate) fn parse_unix_dbus_path(addr: &str) -> Option<PathBuf> {
    for entry in addr.split(';') {
        let Some(rest) = entry.trim().strip_prefix("unix:") else {
            continue;
        };
        for pair in rest.split(',') {
            let Some((key, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            match key {
                "path" => {
                    let path = percent_decode(value);
                    if path.starts_with(b"/") && !path.contains(&0) {
                        return Some(PathBuf::from(std::ffi::OsString::from_vec(path)));
                    }
                }
                "abstract" => {
                    let mut path = vec![0];
                    path.extend(percent_decode(value));
                    return Some(PathBuf::from(std::ffi::OsString::from_vec(path)));
                }
                _ => {}
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex, 16)
        {
            out.push(val);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

// ---- Minimal first-party pure-Rust D-Bus 1.0 Unix-socket wire client --------

struct DbusConn {
    stream: UnixStream,
    next_serial: u32,
    guid: String,
}

async fn read_crlf_line(stream: &mut UnixStream) -> Result<String, AdapterError> {
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() >= 4096 {
            return Err(AdapterError::Backend(
                "D-Bus SASL line exceeded 4 KiB".into(),
            ));
        }
        stream
            .read_exact(&mut byte)
            .await
            .map_err(|e| AdapterError::Backend(format!("D-Bus auth read: {e}")))?;
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

impl DbusConn {
    async fn connect(path: &Path) -> Result<Self, AdapterError> {
        let stream = tokio::time::timeout(DBUS_TIMEOUT, UnixStream::connect(path))
            .await
            .map_err(|_| {
                AdapterError::Backend(format!("D-Bus connect timeout: {}", path.display()))
            })?
            .map_err(|e| {
                if e.kind() == ErrorKind::PermissionDenied {
                    AdapterError::PermissionDenied(format!(
                        "D-Bus socket {} permission denied: {e}",
                        path.display()
                    ))
                } else {
                    AdapterError::Backend(format!("D-Bus connect {}: {e}", path.display()))
                }
            })?;
        let mut conn = Self {
            stream,
            next_serial: 1,
            guid: String::new(),
        };
        conn.authenticate().await?;
        // Register on the bus via org.freedesktop.DBus.Hello.
        let _ = conn
            .call_raw(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "Hello",
                "",
                &[],
            )
            .await?;
        Ok(conn)
    }

    async fn authenticate(&mut self) -> Result<(), AdapterError> {
        let uid = current_uid_decimal();
        let uid_hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        let auth_cmd = format!("\0AUTH EXTERNAL {uid_hex}\r\n");
        self.stream
            .write_all(auth_cmd.as_bytes())
            .await
            .map_err(|e| AdapterError::Backend(format!("D-Bus auth write: {e}")))?;

        let line = tokio::time::timeout(DBUS_TIMEOUT, read_crlf_line(&mut self.stream))
            .await
            .map_err(|_| AdapterError::Backend("D-Bus auth reply timeout".into()))??;

        if line.starts_with("REJECTED") {
            return Err(AdapterError::PermissionDenied(format!(
                "D-Bus SASL EXTERNAL authentication rejected: {}",
                line.trim()
            )));
        }
        if !line.starts_with("OK ") {
            return Err(AdapterError::Backend(format!(
                "unexpected D-Bus auth reply: {}",
                line.trim()
            )));
        }
        self.guid = line.trim()[3..].to_owned();
        self.stream
            .write_all(b"BEGIN\r\n")
            .await
            .map_err(|e| AdapterError::Backend(format!("D-Bus BEGIN write: {e}")))?;
        Ok(())
    }

    async fn call_raw(
        &mut self,
        dest: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<(String, Vec<u8>), AdapterError> {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        let msg = build_method_call(serial, dest, path, interface, member, signature, body);
        tokio::time::timeout(DBUS_TIMEOUT, self.stream.write_all(&msg))
            .await
            .map_err(|_| AdapterError::Backend("D-Bus send timeout".into()))?
            .map_err(|e| AdapterError::Backend(format!("D-Bus write: {e}")))?;

        tokio::time::timeout(DBUS_TIMEOUT, async {
            loop {
                let reply = read_dbus_message(&mut self.stream).await?;
                if reply.reply_serial == Some(serial) {
                    return match reply.msg_type {
                        2 => Ok((reply.signature, reply.body)),
                        3 => {
                            let detail = decode_string(&reply.body, &mut 0)
                                .unwrap_or_else(|| reply.error_name.clone());
                            if reply.error_name.contains("AccessDenied") {
                                Err(AdapterError::PermissionDenied(detail))
                            } else if reply.error_name == "org.freedesktop.DBus.Error.UnknownMethod"
                                || reply.error_name == "org.freedesktop.DBus.Error.UnknownInterface"
                            {
                                Err(AdapterError::UnsupportedAction(format!(
                                    "{interface}.{member}: {detail}"
                                )))
                            } else if reply.error_name == "org.freedesktop.DBus.Error.UnknownObject"
                                || (dest.starts_with(':')
                                    && reply.error_name
                                        == "org.freedesktop.DBus.Error.NameHasNoOwner")
                            {
                                Err(AdapterError::StaleRef(format!(
                                    "accessible object no longer exists: {detail}"
                                )))
                            } else {
                                Err(AdapterError::Backend(format!(
                                    "{}: {detail}",
                                    reply.error_name
                                )))
                            }
                        }
                        other => Err(AdapterError::Backend(format!(
                            "unexpected D-Bus reply type {other}"
                        ))),
                    };
                }
            }
        })
        .await
        .map_err(|_| AdapterError::Backend(format!("D-Bus timeout calling {interface}.{member}")))?
    }

    async fn call_string(
        &mut self,
        dest: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<String, AdapterError> {
        let (_, resp) = self
            .call_raw(dest, path, interface, member, signature, body)
            .await?;
        let mut cursor = 0;
        decode_string(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend(format!("invalid string reply for {member}")))
    }

    async fn call_bool(
        &mut self,
        dest: &str,
        path: &str,
        interface: &str,
        member: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<bool, AdapterError> {
        let (_, resp) = self
            .call_raw(dest, path, interface, member, signature, body)
            .await?;
        let mut cursor = 0;
        decode_u32(&resp, &mut cursor)
            .map(|v| v != 0)
            .ok_or_else(|| AdapterError::Backend(format!("invalid bool reply for {member}")))
    }

    async fn get_string_prop(
        &mut self,
        dest: &str,
        path: &str,
        prop_iface: &str,
        prop_name: &str,
    ) -> Result<String, AdapterError> {
        let mut body = Vec::new();
        encode_string(&mut body, prop_iface);
        encode_string(&mut body, prop_name);
        let (_, resp) = self
            .call_raw(
                dest,
                path,
                "org.freedesktop.DBus.Properties",
                "Get",
                "ss",
                &body,
            )
            .await?;
        let mut cursor = 0;
        let sig = decode_signature(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("missing variant signature".into()))?;
        if sig != "s" {
            return Err(AdapterError::Backend(format!(
                "expected string property {prop_name}, got signature {sig:?}"
            )));
        }
        decode_string(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("truncated string variant".into()))
    }

    async fn get_i32_prop(
        &mut self,
        dest: &str,
        path: &str,
        prop_iface: &str,
        prop_name: &str,
    ) -> Result<i32, AdapterError> {
        let mut body = Vec::new();
        encode_string(&mut body, prop_iface);
        encode_string(&mut body, prop_name);
        let (_, resp) = self
            .call_raw(
                dest,
                path,
                "org.freedesktop.DBus.Properties",
                "Get",
                "ss",
                &body,
            )
            .await?;
        let mut cursor = 0;
        let sig = decode_signature(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("missing variant signature".into()))?;
        if sig != "i" {
            return Err(AdapterError::Backend(format!(
                "expected i32 property {prop_name}, got signature {sig:?}"
            )));
        }
        decode_i32(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("truncated i32 variant".into()))
    }

    async fn get_child_at_index(
        &mut self,
        dest: &str,
        path: &str,
        index: i32,
    ) -> Result<(String, String), AdapterError> {
        let mut body = Vec::new();
        encode_i32(&mut body, index);
        let (_, resp) = self
            .call_raw(
                dest,
                path,
                "org.a11y.atspi.Accessible",
                "GetChildAtIndex",
                "i",
                &body,
            )
            .await?;
        let mut cursor = 0;
        align_cursor(&mut cursor, 8);
        let child_bus = decode_string(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("missing child bus name".into()))?;
        let child_path = decode_string(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("missing child object path".into()))?;
        Ok((child_bus, child_path))
    }

    async fn get_state(&mut self, dest: &str, path: &str) -> Result<Vec<u32>, AdapterError> {
        let (_, resp) = self
            .call_raw(dest, path, "org.a11y.atspi.Accessible", "GetState", "", &[])
            .await?;
        let mut cursor = 0;
        let byte_len = decode_u32(&resp, &mut cursor)
            .ok_or_else(|| AdapterError::Backend("missing state array length".into()))?
            as usize;
        let end = cursor.saturating_add(byte_len).min(resp.len());
        let mut words = Vec::new();
        while cursor + 4 <= end {
            if let Some(w) = decode_u32(&resp, &mut cursor) {
                words.push(w);
            }
        }
        Ok(words)
    }

    async fn get_extents(
        &mut self,
        dest: &str,
        path: &str,
    ) -> Result<Option<DesktopRect>, AdapterError> {
        let mut body = Vec::new();
        encode_u32(&mut body, 0); // ATSPI_COORD_TYPE_SCREEN
        let (_, resp) = self
            .call_raw(
                dest,
                path,
                "org.a11y.atspi.Component",
                "GetExtents",
                "u",
                &body,
            )
            .await?;
        let mut cursor = 0;
        align_cursor(&mut cursor, 8);
        let Some(x) = decode_i32(&resp, &mut cursor) else {
            return Ok(None);
        };
        let Some(y) = decode_i32(&resp, &mut cursor) else {
            return Ok(None);
        };
        let Some(w) = decode_i32(&resp, &mut cursor) else {
            return Ok(None);
        };
        let Some(h) = decode_i32(&resp, &mut cursor) else {
            return Ok(None);
        };
        if w <= 0 || h <= 0 {
            return Ok(None);
        }
        Ok(Some(DesktopRect {
            x,
            y,
            width: w as u32,
            height: h as u32,
        }))
    }
}

async fn walk_atspi_root(
    conn: &mut DbusConn,
    max_depth: usize,
    max_nodes: usize,
    pid_filter: Option<u32>,
) -> Result<Vec<RawNode<LinuxHandle>>, AdapterError> {
    walk_atspi_apps(
        conn,
        pid_filter,
        0,
        &mut Walk::new(max_depth, max_nodes.saturating_add(1), false),
    )
    .await
}

struct Walk {
    depth: usize,
    remaining: usize,
    truncated: bool,
    strict: bool,
}

impl Walk {
    fn new(depth: usize, remaining: usize, strict: bool) -> Self {
        Self {
            depth,
            remaining,
            truncated: false,
            strict,
        }
    }

    fn take(&mut self) -> bool {
        if self.remaining == 0 {
            self.truncated = true;
            false
        } else {
            self.remaining -= 1;
            true
        }
    }
}

async fn walk_atspi_apps(
    conn: &mut DbusConn,
    pid_filter: Option<u32>,
    offset: usize,
    walk: &mut Walk,
) -> Result<Vec<RawNode<LinuxHandle>>, AdapterError> {
    let root_bus = "org.a11y.atspi.Registry";
    let root_path = "/org/a11y/atspi/accessible/root";
    let app_count = conn
        .get_i32_prop(
            root_bus,
            root_path,
            "org.a11y.atspi.Accessible",
            "ChildCount",
        )
        .await?
        .max(0) as usize;

    walk.truncated |= offset > 0 && app_count > 0;
    let mut roots = Vec::new();
    for idx in offset..app_count {
        if !walk.take() {
            break;
        }
        let Ok((app_bus, app_path)) = conn
            .get_child_at_index(root_bus, root_path, idx as i32)
            .await
        else {
            walk.truncated = true;
            continue;
        };
        if app_bus.is_empty()
            || app_path == "/org/a11y/atspi/null"
            || (walk.strict && !app_bus.starts_with(':'))
        {
            walk.truncated = true;
            continue;
        }
        let pid = conn
            .call_raw(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetConnectionUnixProcessID",
                "s",
                &encoded_string(&app_bus),
            )
            .await
            .ok()
            .and_then(|(_, b)| decode_u32(&b, &mut 0));
        if pid_filter.is_some() && pid != pid_filter {
            continue;
        }
        if let Some(node) = walk_atspi_node(conn, &app_bus, &app_path, None, pid, 1, 0, walk).await
        {
            roots.push(node);
        }
    }
    Ok(roots)
}

#[allow(clippy::too_many_arguments)]
fn walk_atspi_node<'a>(
    conn: &'a mut DbusConn,
    bus: &'a str,
    path: &'a str,
    app_id: Option<String>,
    pid: Option<u32>,
    depth: usize,
    offset: usize,
    walk: &'a mut Walk,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<RawNode<LinuxHandle>>> + Send + 'a>>
{
    Box::pin(async move {
        let role = conn
            .call_string(
                bus,
                path,
                "org.a11y.atspi.Accessible",
                "GetRoleName",
                "",
                &[],
            )
            .await;
        let role = match role {
            Ok(role) => role.replace(' ', "_"),
            Err(_) if walk.strict => {
                walk.truncated = true;
                return None;
            }
            Err(_) => "unknown".into(),
        };
        let name = conn
            .get_string_prop(bus, path, "org.a11y.atspi.Accessible", "Name")
            .await
            .unwrap_or_default();
        let states = conn.get_state(bus, path).await.unwrap_or_default();
        let enabled = if states.is_empty() {
            true
        } else {
            has_state(&states, ATSPI_STATE_ENABLED)
        };
        let focused = has_state(&states, ATSPI_STATE_FOCUSED);
        let bounds = conn.get_extents(bus, path).await.ok().flatten();
        let role = limit_snapshot_string(role, walk.strict);
        let name = limit_snapshot_string(name, walk.strict);
        let effective_app =
            app_id.or_else(|| (role == "application" && !name.is_empty()).then(|| name.clone()));

        let mut children = Vec::new();
        let count = match conn
            .get_i32_prop(bus, path, "org.a11y.atspi.Accessible", "ChildCount")
            .await
        {
            Ok(count) => count.max(0) as usize,
            Err(_) => {
                walk.truncated = true;
                0
            }
        };
        walk.truncated |= offset > 0 && count > 0;
        let can_descend = if walk.strict {
            depth < walk.depth
        } else {
            depth <= walk.depth
        };
        if can_descend {
            for idx in offset..count {
                if !walk.take() {
                    break;
                }
                let Ok((child_bus, child_path)) =
                    conn.get_child_at_index(bus, path, idx as i32).await
                else {
                    walk.truncated = true;
                    continue;
                };
                if child_path == "/org/a11y/atspi/null"
                    || (walk.strict && !child_bus.is_empty() && !child_bus.starts_with(':'))
                {
                    walk.truncated = true;
                    continue;
                }
                let target_bus = if child_bus.is_empty() {
                    bus
                } else {
                    &child_bus
                };
                if let Some(child) = walk_atspi_node(
                    conn,
                    target_bus,
                    &child_path,
                    effective_app.clone(),
                    pid,
                    depth + 1,
                    0,
                    walk,
                )
                .await
                {
                    children.push(child);
                }
            }
        } else if count > offset {
            walk.truncated = true;
        }

        Some(RawNode {
            handle: LinuxHandle::AtSpi {
                bus: bus.to_owned(),
                path: path.to_owned(),
            },
            role,
            name,
            value: None,
            app_id: effective_app,
            pid,
            enabled,
            focused,
            bounds,
            actions: Vec::new(),
            children,
        })
    })
}

fn limit_snapshot_string(mut text: String, strict: bool) -> String {
    if strict && text.len() > 1024 {
        if let Some(end) = (0..=1024).rev().find(|&i| text.is_char_boundary(i)) {
            text.truncate(end);
        }
        text.push('…');
    }
    text
}

fn has_state(words: &[u32], bit: u32) -> bool {
    let idx = (bit / 32) as usize;
    let mask = 1u32 << (bit % 32);
    words.get(idx).is_some_and(|w| (*w & mask) != 0)
}

fn current_uid_decimal() -> String {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:")
                && let Some(uid) = rest.split_whitespace().next()
            {
                return uid.to_owned();
            }
        }
    }
    "1000".into()
}

struct DbusReply {
    msg_type: u8,
    reply_serial: Option<u32>,
    error_name: String,
    signature: String,
    body: Vec<u8>,
}

fn pad_to(buf: &mut Vec<u8>, align: usize) {
    let rem = buf.len() % align;
    if rem != 0 {
        buf.resize(buf.len() + (align - rem), 0);
    }
}

fn align_cursor(cursor: &mut usize, align: usize) {
    let rem = *cursor % align;
    if rem != 0 {
        *cursor += align - rem;
    }
}

fn encode_u32(buf: &mut Vec<u8>, v: u32) {
    pad_to(buf, 4);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn encode_i32(buf: &mut Vec<u8>, v: i32) {
    pad_to(buf, 4);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn encode_string(buf: &mut Vec<u8>, s: &str) {
    encode_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn encoded_string(s: &str) -> Vec<u8> {
    let mut b = Vec::new();
    encode_string(&mut b, s);
    b
}

fn encode_signature(buf: &mut Vec<u8>, sig: &str) {
    buf.push(sig.len() as u8);
    buf.extend_from_slice(sig.as_bytes());
    buf.push(0);
}

fn encode_header_field_str(fields: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
    pad_to(fields, 8);
    fields.push(code);
    encode_signature(fields, sig);
    if sig == "g" {
        encode_signature(fields, value);
    } else {
        encode_string(fields, value);
    }
}

fn build_method_call(
    serial: u32,
    dest: &str,
    path: &str,
    interface: &str,
    member: &str,
    signature: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut fields = Vec::new();
    encode_header_field_str(&mut fields, 1, "o", path);
    encode_header_field_str(&mut fields, 2, "s", interface);
    encode_header_field_str(&mut fields, 3, "s", member);
    encode_header_field_str(&mut fields, 6, "s", dest);
    if !signature.is_empty() {
        encode_header_field_str(&mut fields, 8, "g", signature);
    }
    let mut msg = Vec::with_capacity(16 + fields.len() + body.len());
    msg.extend_from_slice(&[b'l', 1, 0, 1]);
    msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
    msg.extend_from_slice(&serial.to_le_bytes());
    msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
    msg.extend_from_slice(&fields);
    pad_to(&mut msg, 8);
    msg.extend_from_slice(body);
    msg
}

fn decode_u32(buf: &[u8], cursor: &mut usize) -> Option<u32> {
    align_cursor(cursor, 4);
    let slice = buf.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn decode_i32(buf: &[u8], cursor: &mut usize) -> Option<i32> {
    decode_u32(buf, cursor).map(|v| v as i32)
}

fn decode_string(buf: &[u8], cursor: &mut usize) -> Option<String> {
    let len = decode_u32(buf, cursor)? as usize;
    let bytes = buf.get(*cursor..*cursor + len)?;
    *cursor += len + 1; // skip trailing NUL
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn decode_signature(buf: &[u8], cursor: &mut usize) -> Option<String> {
    let len = *buf.get(*cursor)? as usize;
    *cursor += 1;
    let bytes = buf.get(*cursor..*cursor + len)?;
    *cursor += len + 1;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

async fn read_dbus_message(stream: &mut UnixStream) -> Result<DbusReply, AdapterError> {
    let mut fixed = [0u8; 16];
    stream
        .read_exact(&mut fixed)
        .await
        .map_err(|e| AdapterError::Backend(format!("D-Bus read header: {e}")))?;
    if fixed[0] != b'l' {
        return Err(AdapterError::Backend(
            "unsupported non-little-endian D-Bus peer".into(),
        ));
    }
    let msg_type = fixed[1];
    let body_len = u32::from_le_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]) as usize;
    let fields_len = u32::from_le_bytes([fixed[12], fixed[13], fixed[14], fixed[15]]) as usize;
    if body_len > MAX_DBUS_BODY || fields_len > 1024 * 1024 {
        return Err(AdapterError::Backend(
            "D-Bus message exceeds size bound".into(),
        ));
    }
    let header_total = 16 + fields_len;
    let pad = (8 - (header_total % 8)) % 8;
    let mut fields_and_pad = vec![0u8; fields_len + pad];
    stream
        .read_exact(&mut fields_and_pad)
        .await
        .map_err(|e| AdapterError::Backend(format!("D-Bus read fields: {e}")))?;
    let mut body = vec![0u8; body_len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|e| AdapterError::Backend(format!("D-Bus read body: {e}")))?;

    let mut reply_serial = None;
    let mut error_name = String::new();
    let mut signature = String::new();
    let mut cursor = 0;
    while cursor < fields_len {
        align_cursor(&mut cursor, 8);
        if cursor >= fields_len {
            break;
        }
        let code = fields_and_pad[cursor];
        cursor += 1;
        let Some(sig) = decode_signature(&fields_and_pad, &mut cursor) else {
            break;
        };
        match (code, sig.as_str()) {
            (4, "s") => {
                if let Some(s) = decode_string(&fields_and_pad, &mut cursor) {
                    error_name = s;
                }
            }
            (5, "u") => {
                reply_serial = decode_u32(&fields_and_pad, &mut cursor);
            }
            (8, "g") => {
                if let Some(s) = decode_signature(&fields_and_pad, &mut cursor) {
                    signature = s;
                }
            }
            (_, "s" | "o") => {
                let _ = decode_string(&fields_and_pad, &mut cursor);
            }
            (_, "g") => {
                let _ = decode_signature(&fields_and_pad, &mut cursor);
            }
            (_, "u" | "i" | "b") => {
                let _ = decode_u32(&fields_and_pad, &mut cursor);
            }
            _ => break,
        }
    }
    Ok(DbusReply {
        msg_type,
        reply_serial,
        error_name,
        signature,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::NodeRef;
    use tokio::net::UnixListener;

    #[test]
    fn parse_unix_dbus_path_decodes_standard_and_escaped_paths() {
        assert_eq!(
            parse_unix_dbus_path("unix:path=/run/user/1000/at-spi/bus_0,guid=abc"),
            Some(PathBuf::from("/run/user/1000/at-spi/bus_0"))
        );
        assert_eq!(
            parse_unix_dbus_path("unix:path=/tmp/my%20bus/sock"),
            Some(PathBuf::from("/tmp/my bus/sock"))
        );
        assert_eq!(parse_unix_dbus_path("tcp:host=127.0.0.1,port=1234"), None);
    }

    fn build_reply(reply_serial: u32, sig: &str, body: &[u8]) -> Vec<u8> {
        let mut fields = Vec::new();
        pad_to(&mut fields, 8);
        fields.push(5); // REPLY_SERIAL
        encode_signature(&mut fields, "u");
        encode_u32(&mut fields, reply_serial);
        if !sig.is_empty() {
            encode_header_field_str(&mut fields, 8, "g", sig);
        }
        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 2, 0, 1]);
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&99u32.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(body);
        msg
    }

    async fn read_client_call(stream: &mut UnixStream) -> Option<(u32, String, String, Vec<u8>)> {
        let mut fixed = [0u8; 16];
        stream.read_exact(&mut fixed).await.ok()?;
        let body_len = u32::from_le_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]) as usize;
        let serial = u32::from_le_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]);
        let fields_len = u32::from_le_bytes([fixed[12], fixed[13], fixed[14], fixed[15]]) as usize;
        let pad = (8 - ((16 + fields_len) % 8)) % 8;
        let mut fields = vec![0u8; fields_len + pad];
        stream.read_exact(&mut fields).await.ok()?;
        let mut body = vec![0u8; body_len];
        stream.read_exact(&mut body).await.ok()?;

        let mut path = String::new();
        let mut member = String::new();
        let mut c = 0;
        while c < fields_len {
            align_cursor(&mut c, 8);
            if c >= fields_len {
                break;
            }
            let code = fields[c];
            c += 1;
            let sig = decode_signature(&fields, &mut c)?;
            match (code, sig.as_str()) {
                (1, "o") => path = decode_string(&fields, &mut c)?,
                (3, "s") => member = decode_string(&fields, &mut c)?,
                (_, "s" | "o") => {
                    let _ = decode_string(&fields, &mut c)?;
                }
                (_, "g") => {
                    let _ = decode_signature(&fields, &mut c)?;
                }
                _ => break,
            }
        }
        Some((serial, path, member, body))
    }

    #[tokio::test]
    async fn linux_atspi2_wire_client_walks_tree_and_dispatches_actions() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("atspi.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let received_actions = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let actions_ref = std::sync::Arc::clone(&received_actions);

        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let actions_ref = std::sync::Arc::clone(&actions_ref);
                tokio::spawn(async move {
                    // SASL handshake (byte-exact, never over-reading into binary frames)
                    let mut nul = [0u8; 1];
                    if stream.read_exact(&mut nul).await.is_err() {
                        return;
                    }
                    if read_crlf_line(&mut stream).await.is_err() {
                        return;
                    }
                    let _ = stream.write_all(b"OK deadbeef\r\n").await;
                    if read_crlf_line(&mut stream).await.is_err() {
                        return;
                    }

                    while let Some((serial, path, member, body)) =
                        read_client_call(&mut stream).await
                    {
                        let (sig, resp_body) = match member.as_str() {
                            "Hello" => ("s", encoded_string(":1.42")),
                            "GetConnectionUnixProcessID" => {
                                let mut b = Vec::new();
                                encode_u32(&mut b, 777);
                                ("u", b)
                            }
                            "Get" => {
                                let mut c = 0;
                                let _iface = decode_string(&body, &mut c).unwrap();
                                let prop = decode_string(&body, &mut c).unwrap();
                                let mut b = Vec::new();
                                match (path.as_str(), prop.as_str()) {
                                    ("/org/a11y/atspi/accessible/root", "ChildCount") => {
                                        encode_signature(&mut b, "i");
                                        encode_i32(&mut b, 1);
                                    }
                                    ("/app/1", "ChildCount") => {
                                        encode_signature(&mut b, "i");
                                        encode_i32(&mut b, 1);
                                    }
                                    ("/app/1", "Name") => {
                                        encode_signature(&mut b, "s");
                                        encode_string(&mut b, "Notes");
                                    }
                                    ("/btn/1", "ChildCount") => {
                                        encode_signature(&mut b, "i");
                                        encode_i32(&mut b, 0);
                                    }
                                    ("/btn/1", "Name") => {
                                        encode_signature(&mut b, "s");
                                        encode_string(&mut b, "Save");
                                    }
                                    _ => {
                                        encode_signature(&mut b, "s");
                                        encode_string(&mut b, "");
                                    }
                                }
                                ("v", b)
                            }
                            "GetChildAtIndex" => {
                                let mut b = Vec::new();
                                pad_to(&mut b, 8);
                                if path == "/org/a11y/atspi/accessible/root" {
                                    encode_string(&mut b, ":1.10");
                                    encode_string(&mut b, "/app/1");
                                } else {
                                    encode_string(&mut b, ":1.10");
                                    encode_string(&mut b, "/btn/1");
                                }
                                ("(so)", b)
                            }
                            "GetRoleName" => {
                                let role = if path == "/app/1" {
                                    "application"
                                } else {
                                    "push button"
                                };
                                ("s", encoded_string(role))
                            }
                            "GetState" => {
                                let mut b = Vec::new();
                                encode_u32(&mut b, 4);
                                encode_u32(
                                    &mut b,
                                    (1 << ATSPI_STATE_ENABLED) | (1 << ATSPI_STATE_FOCUSED),
                                );
                                ("au", b)
                            }
                            "GetExtents" => {
                                let mut b = Vec::new();
                                pad_to(&mut b, 8);
                                encode_i32(&mut b, 10);
                                encode_i32(&mut b, 20);
                                encode_i32(&mut b, 100);
                                encode_i32(&mut b, 40);
                                ("(iiii)", b)
                            }
                            "DoAction" | "GrabFocus" | "SetTextContents" => {
                                actions_ref.lock().unwrap().push(format!("{member}:{path}"));
                                let mut b = Vec::new();
                                encode_u32(&mut b, 1);
                                ("b", b)
                            }
                            _ => ("", Vec::new()),
                        };
                        let reply = build_reply(serial, sig, &resp_body);
                        if stream.write_all(&reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let adapter = LinuxPlatformAdapter::with_sockets(Some(sock), None, None, (1280, 800));
        let tree = adapter.snapshot(TreeOptions::default()).await.unwrap();
        assert_eq!(tree.snapshot_id, "p1");
        assert_eq!(tree.platform, "linux");
        assert_eq!(tree.total_nodes, 2);
        assert_eq!(tree.find_node("@p1:e1").unwrap().role, "application");
        assert_eq!(tree.find_node("@p1:e1").unwrap().name, "Notes");
        assert_eq!(tree.find_node("@p1:e2").unwrap().role, "push_button");
        assert_eq!(tree.find_node("@p1:e2").unwrap().name, "Save");
        assert_eq!(
            tree.find_node("@p1:e2").unwrap().bounds,
            Some(DesktopRect {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            })
        );

        adapter
            .act(DesktopAction::Click {
                target: NodeRef::parse("@p1:e2").unwrap(),
                button: None,
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::Focus {
                target: NodeRef::parse("@p1:e2").unwrap(),
            })
            .await
            .unwrap();
        adapter
            .act(DesktopAction::SetValue {
                target: NodeRef::parse("@p1:e2").unwrap(),
                value: "ok".into(),
            })
            .await
            .unwrap();

        // Snapshot rotation invalidates @p1:e2 with StaleRef.
        let _t2 = adapter.snapshot(TreeOptions::default()).await.unwrap();
        let stale = adapter
            .act(DesktopAction::Click {
                target: NodeRef::parse("@p1:e2").unwrap(),
                button: None,
            })
            .await
            .unwrap_err();
        assert_eq!(stale.code(), "stale-ref");

        assert_eq!(
            *received_actions.lock().unwrap(),
            vec![
                "DoAction:/btn/1",
                "GrabFocus:/btn/1",
                "SetTextContents:/btn/1",
            ]
        );
        server.abort();
    }

    #[tokio::test]
    async fn dbus_sasl_rejection_fails_closed_with_permission_denied() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("denied.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut nul = [0u8; 1];
                let _ = stream.read_exact(&mut nul).await;
                let _ = read_crlf_line(&mut stream).await;
                let _ = stream.write_all(b"REJECTED EXTERNAL\r\n").await;
            }
        });

        let adapter = LinuxPlatformAdapter::with_sockets(Some(sock), None, None, (1280, 800));
        let err = adapter.snapshot(TreeOptions::default()).await.unwrap_err();
        assert_eq!(err.code(), "permission-denied");
        server.abort();
    }
}
