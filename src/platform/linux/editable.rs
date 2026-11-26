//! A metadata-only AT-SPI probe for mobile takeover, separate from the named
//! accessibility tree used for agent desktop actions.

use super::{
    ATSPI_STATE_ENABLED, ATSPI_STATE_FOCUSED, DbusConn, LinuxPlatformAdapter, decode_u32,
    encoded_string, has_state,
};
use crate::desktop::DesktopRect;
use crate::takeover::{InputContext, InputPurpose};
use std::collections::HashSet;
use std::time::Duration;

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const WINDOW_COORDS: u32 = 1;
const MAX_NODES: usize = 512;
const MAX_WINDOWS: usize = 64;
const TIMEOUT: Duration = Duration::from_millis(400);

// org.a11y.atspi.StateType and Role.
const ACTIVE: u32 = 1;
const DEFUNCT: u32 = 6;
const EDITABLE: u32 = 7;
const ICONIFIED: u32 = 15;
const SENSITIVE: u32 = 24;
const SHOWING: u32 = 25;
const STALE: u32 = 27;
const VISIBLE: u32 = 30;
const SINGLE_LINE: u32 = 26;
const SUPPORTS_AUTOCOMPLETION: u32 = 37;
const READ_ONLY: u32 = 43;
const SPIN_BUTTON: u32 = 52;
const ENTRY: u32 = 79;

impl LinuxPlatformAdapter {
    /// Inspect the compositor-focused window, without collecting accessible
    /// names, text, values, cache items or general attribute maps.
    pub(crate) async fn editable_context(
        &self,
        window: [i32; 4],
        pid: Option<u32>,
        browser: bool,
    ) -> Option<InputContext> {
        let pid = pid?;
        if window[2] <= 0 || window[3] <= 0 {
            return None;
        }
        tokio::time::timeout(TIMEOUT, async {
            let mut conn = self.connect_atspi().await.ok()?;
            probe(&mut conn, window, pid, browser).await
        })
        .await
        .ok()
        .flatten()
    }
}

async fn probe(
    conn: &mut DbusConn,
    window: [i32; 4],
    pid: u32,
    browser: bool,
) -> Option<InputContext> {
    let (bus, toplevel, origin) = focused_window(conn, window, pid, browser).await?;
    let mut context = InputContext {
        active: false,
        purpose: InputPurpose::Text,
        rects: Vec::new(),
    };
    let mut pending = vec![toplevel];
    let mut visited = HashSet::new();
    let mut focused = None;
    while let Some(path) = pending.pop() {
        if !visited.insert(path.clone()) {
            continue;
        }
        let states = conn.get_state(&bus, &path).await.ok()?;
        if unavailable(&states) {
            continue;
        }
        let writable = writable(&states);
        let role = if browser || writable {
            role(conn, &bus, &path).await?
        } else {
            0
        };
        // Do not traverse Chromium's web accessibility tree; CDP already
        // measures it with the browser viewport/surface offsets.
        if browser && matches!(role, 82 | 92..=96) {
            continue;
        }
        if writable
            && let Some(bounds) = conn
                .get_extents_in(&bus, &path, WINDOW_COORDS)
                .await
                .ok()
                .flatten()
            && let Some(rect) = buffer_rect(bounds, origin, window)
        {
            if !context.rects.contains(&rect) {
                context.rects.push(rect);
            }
            if has_state(&states, ATSPI_STATE_FOCUSED) {
                if focused.is_some() {
                    return None;
                }
                focused = Some((path.clone(), purpose(role, &states, browser)));
            }
        }
        let remaining = MAX_NODES.saturating_sub(visited.len() + pending.len());
        let paths = conn
            .accessible_children(&bus, &path, remaining)
            .await
            .ok()?;
        pending.extend(paths.into_iter().rev().filter_map(|(child_bus, child)| {
            (child_bus == bus || child_bus.is_empty()).then_some(child)
        }));
    }
    if let Some((path, purpose)) = focused {
        // The full walk is not atomic. Recheck the winning field after the
        // other reads so a stale FOCUSED bit cannot open the keyboard.
        let states = conn.get_state(&bus, &path).await.ok()?;
        if writable(&states) && has_state(&states, ATSPI_STATE_FOCUSED) {
            context.active = true;
            context.purpose = purpose;
        }
    }
    Some(context)
}

struct WindowMatch {
    score: (bool, bool),
    bus: String,
    path: String,
    origin: [i64; 2],
}

async fn focused_window(
    conn: &mut DbusConn,
    window: [i32; 4],
    pid: u32,
    browser: bool,
) -> Option<(String, String, [i64; 2])> {
    let mut best: Option<WindowMatch> = None;
    let mut ambiguous = false;
    for (bus, root) in conn
        .accessible_children("org.a11y.atspi.Registry", ROOT, MAX_WINDOWS)
        .await
        .ok()?
    {
        if !bus.starts_with(':') || process_id(conn, &bus).await != Some(pid) {
            continue;
        }
        for (child_bus, frame) in conn
            .accessible_children(&bus, &root, MAX_WINDOWS)
            .await
            .ok()?
        {
            if !child_bus.is_empty() && child_bus != bus {
                continue;
            }
            let states = conn.get_state(&bus, &frame).await.ok()?;
            if unavailable(&states) || !has_state(&states, SHOWING) || !has_state(&states, VISIBLE)
            {
                continue;
            }
            let Some(bounds) = conn
                .get_extents_in(&bus, &frame, WINDOW_COORDS)
                .await
                .ok()
                .flatten()
            else {
                continue;
            };
            let Some((origin, exact)) = frame_origin(bounds, window, browser) else {
                continue;
            };
            let score = (has_state(&states, ACTIVE), exact);
            match &best {
                Some(previous) if score == previous.score => ambiguous = true,
                Some(previous) if score < previous.score => (),
                _ => {
                    best = Some(WindowMatch {
                        score,
                        bus: bus.clone(),
                        path: frame,
                        origin,
                    });
                    ambiguous = false;
                }
            }
        }
    }
    if ambiguous {
        return None;
    }
    best.map(|m| (m.bus, m.path, m.origin))
}

fn frame_origin(bounds: DesktopRect, window: [i32; 4], browser: bool) -> Option<([i64; 2], bool)> {
    let dx = i64::from(bounds.width) - i64::from(window[2]);
    let dy = i64::from(bounds.height) - i64::from(window[3]);
    // WINDOW coordinates are local on Wayland, even if GetExtents(SCREEN)
    // returns the same local coordinates. Chromium includes symmetric client
    // shadows in its accessible frame, but sway's window rect excludes them.
    // Take the smaller excess (a narrow tiled window can also clip its client).
    let excess = dx.min(dy);
    let exact = dx == 0 && dy == 0;
    if !exact && !(browser && (0..=64).contains(&excess) && excess % 2 == 0 && dx.max(dy) <= 128) {
        return None;
    }
    let inset = excess / 2;
    Some((
        [i64::from(bounds.x) + inset, i64::from(bounds.y) + inset],
        exact,
    ))
}

fn buffer_rect(bounds: DesktopRect, origin: [i64; 2], window: [i32; 4]) -> Option<[i32; 4]> {
    let x = i64::from(bounds.x) - origin[0];
    let y = i64::from(bounds.y) - origin[1];
    let left = x.max(0);
    let top = y.max(0);
    let right = (x + i64::from(bounds.width)).min(i64::from(window[2]));
    let bottom = (y + i64::from(bounds.height)).min(i64::from(window[3]));
    if left >= right || top >= bottom {
        return None;
    }
    Some([
        i32::try_from(i64::from(window[0]) + left).ok()?,
        i32::try_from(i64::from(window[1]) + top).ok()?,
        i32::try_from(right - left).ok()?,
        i32::try_from(bottom - top).ok()?,
    ])
}

fn unavailable(states: &[u32]) -> bool {
    states.is_empty()
        || [DEFUNCT, ICONIFIED, STALE]
            .iter()
            .any(|&bit| has_state(states, bit))
}

fn writable(states: &[u32]) -> bool {
    !unavailable(states)
        && [EDITABLE, ATSPI_STATE_ENABLED, SENSITIVE, SHOWING, VISIBLE]
            .iter()
            .all(|&bit| has_state(states, bit))
        && !has_state(states, READ_ONLY)
}

fn purpose(role: u32, states: &[u32], browser: bool) -> InputPurpose {
    // AT-SPI does not expose the toolkit's input purpose uniformly. The
    // Chromium native single-line autocomplete entry is its omnibox; do not
    // use arbitrary attribute dictionaries (which may contain user text).
    if browser
        && role == ENTRY
        && has_state(states, SINGLE_LINE)
        && has_state(states, SUPPORTS_AUTOCOMPLETION)
    {
        InputPurpose::Url
    } else if role == SPIN_BUTTON {
        InputPurpose::Decimal
    } else {
        InputPurpose::Text
    }
}

async fn role(conn: &mut DbusConn, bus: &str, path: &str) -> Option<u32> {
    let (sig, bytes) = conn
        .call_raw(bus, path, ACCESSIBLE, "GetRole", "", &[])
        .await
        .ok()?;
    (sig == "u").then(|| decode_u32(&bytes, &mut 0)).flatten()
}

async fn process_id(conn: &mut DbusConn, bus: &str) -> Option<u32> {
    let (sig, bytes) = conn
        .call_raw(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            "s",
            &encoded_string(bus),
        )
        .await
        .ok()?;
    (sig == "u").then(|| decode_u32(&bytes, &mut 0)).flatten()
}
