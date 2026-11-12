//! Desktop operations through the Wayland output.
//!
//! - Screenshots: `wlr-screencopy` into a `wl_shm` buffer backed by a memfd,
//!   read back with `pread` (no shared mapping, no unsafe code).
//! - Pointer: `wlr-virtual-pointer` with absolute motion in output pixels.
//! - Keyboard: `virtual-keyboard` with a keymap generated per call that maps
//!   exactly the keysyms the call needs, so any Unicode text types without
//!   depending on the desktop's layout.
//!
//! Screenshots open a short-lived connection; input goes through one
//! [`InputDevice`] per session. All calls block (use `spawn_blocking`).

use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::error::{Error, Result};

const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Evdev codes of character keys (number row and letter rows). Text is typed
/// on these codes only: clients treat codes such as Backspace or Enter by
/// their physical meaning whatever keysym they carry.
const CHAR_CODES: [u32; 47] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 30, 31, 32, 33, 34, 35,
    36, 37, 38, 39, 40, 41, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53,
];
/// Distinct character keysyms one generated keymap carries.
const KEYMAP_SLOTS: usize = CHAR_CODES.len();
const KEY_GAP: Duration = Duration::from_millis(4);

/// A captured desktop frame, RGBA8, top row first.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    /// Encode as PNG.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| Error::Protocol(format!("png: {e}")))?;
        w.write_image_data(&self.rgba).map_err(|e| Error::Protocol(format!("png: {e}")))?;
        w.finish().map_err(|e| Error::Protocol(format!("png: {e}")))?;
        Ok(out)
    }
}

/// Pointer buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    /// Primary button.
    Left,
    /// Middle button.
    Middle,
    /// Secondary button.
    Right,
}

impl Button {
    fn code(self) -> u32 {
        match self {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
        }
    }
}

/// One pointer or keyboard step.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// Move to output pixel coordinates.
    Move { x: f64, y: f64 },
    /// Press or release a button.
    Button { button: Button, pressed: bool },
    /// Scroll by notches (positive = down/right).
    Scroll { dx: i32, dy: i32 },
    /// Press a chord such as `ctrl+l` or `Return`, then release it.
    Key(String),
    /// Type text.
    Text(String),
    /// Pause between steps.
    Wait(Duration),
}

#[derive(Default)]
struct State {
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    flags_y_invert: bool,
    outcome: Option<bool>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(s: &mut Self, _: &ZwlrScreencopyFrameV1, ev: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event as E;
        match ev {
            E::Buffer { format: WEnum::Value(f), width, height, stride } if s.buffer.is_none() => {
                s.buffer = Some((f, width, height, stride));
            }
            E::BufferDone => s.buffer_done = true,
            E::Flags { flags: WEnum::Value(f) } => {
                s.flags_y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            E::Ready { .. } => s.outcome = Some(true),
            E::Failed => s.outcome = Some(false),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ZwlrScreencopyManagerV1);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ZwlrVirtualPointerV1);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ZwpVirtualKeyboardV1);

struct Client {
    conn: Connection,
    queue: EventQueue<State>,
    globals: GlobalList,
    state: State,
}

impl Client {
    fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .map_err(|e| Error::Process(format!("cannot reach the compositor: {e}")))?;
        let conn = Connection::from_socket(stream).map_err(|e| Error::Protocol(format!("wayland: {e}")))?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).map_err(|e| Error::Protocol(format!("wayland registry: {e}")))?;
        Ok(Self { conn, queue, globals, state: State::default() })
    }

    fn bind<I>(&self, versions: std::ops::RangeInclusive<u32>) -> Result<I>
    where
        I: Proxy + 'static,
        State: Dispatch<I, ()>,
    {
        self.globals
            .bind::<I, State, ()>(&self.queue.handle(), versions, ())
            .map_err(|e| Error::Unsupported(format!("compositor lacks {}: {e}", I::interface().name)))
    }

    fn roundtrip(&mut self) -> Result<()> {
        self.queue.roundtrip(&mut self.state).map_err(|e| Error::Protocol(format!("wayland: {e}")))?;
        Ok(())
    }
}

/// Capture the whole output.
pub fn screenshot(socket: &Path) -> Result<Frame> {
    let mut c = Client::connect(socket)?;
    let qh = c.queue.handle();
    let shm: wl_shm::WlShm = c.bind(1..=1)?;
    let output: wl_output::WlOutput = c.bind(1..=4)?;
    let manager: ZwlrScreencopyManagerV1 = c.bind(1..=3)?;
    let frame = manager.capture_output(0, &output, &qh, ());
    let deadline = Instant::now() + FRAME_TIMEOUT;
    while c.state.buffer.is_none() || (frame.version() >= 3 && !c.state.buffer_done) {
        if c.state.outcome == Some(false) || Instant::now() > deadline {
            return Err(Error::Protocol("screencopy offered no shared-memory buffer".into()));
        }
        c.roundtrip()?;
    }
    let (format, width, height, stride) = c.state.buffer.expect("checked above");
    let size = stride as usize * height as usize;
    let fd = rustix::fs::memfd_create("desktop-frame", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| Error::Io(e.into()))?;
    rustix::fs::ftruncate(&fd, size as u64).map_err(|e| Error::Io(e.into()))?;
    let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, &qh, ());
    frame.copy(&buffer);
    while c.state.outcome.is_none() {
        if Instant::now() > deadline {
            return Err(Error::Timeout("screencopy frame".into()));
        }
        c.queue.blocking_dispatch(&mut c.state).map_err(|e| Error::Protocol(format!("wayland: {e}")))?;
    }
    frame.destroy();
    buffer.destroy();
    pool.destroy();
    if c.state.outcome != Some(true) {
        return Err(Error::Protocol("screencopy failed".into()));
    }
    let mut raw = vec![0u8; size];
    std::fs::File::from(fd).read_exact_at(&mut raw, 0)?;
    to_rgba(&raw, format, width, height, stride, c.state.flags_y_invert)
}

fn to_rgba(raw: &[u8], format: wl_shm::Format, width: u32, height: u32, stride: u32, y_invert: bool) -> Result<Frame> {
    // Little-endian 32-bit formats: [A|X]RGB8888 is B,G,R,A in memory; [A|X]BGR8888 is R,G,B,A.
    let bgr = match format {
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => true,
        wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => false,
        other => return Err(Error::Unsupported(format!("screencopy format {other:?}"))),
    };
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height {
        let src = if y_invert { height - 1 - row } else { row } as usize * stride as usize;
        for px in raw[src..src + width as usize * 4].as_chunks::<4>().0 {
            let (r, g, b) = if bgr { (px[2], px[1], px[0]) } else { (px[0], px[1], px[2]) };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Ok(Frame { width, height, rgba })
}

/// A virtual pointer and keyboard held for the life of a session, so the
/// desktop's seat always has both and focused clients stay bound to them.
pub struct InputDevice {
    c: Client,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    size: (u32, u32),
    started: Instant,
}

impl std::fmt::Debug for InputDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InputDevice({}x{})", self.size.0, self.size.1)
    }
}

impl InputDevice {
    /// Create the devices on the output of `size` pixels.
    pub fn open(socket: &Path, size: (u32, u32)) -> Result<Self> {
        let mut c = Client::connect(socket)?;
        let qh = c.queue.handle();
        let seat: wl_seat::WlSeat = c.bind(1..=7)?;
        let pointer = c.bind::<ZwlrVirtualPointerManagerV1>(1..=2)?.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = c.bind::<ZwpVirtualKeyboardManagerV1>(1..=1)?.create_virtual_keyboard(&seat, &qh, ());
        upload_keymap(&keyboard, &[(1, "Escape".to_string())])?;
        c.roundtrip()?;
        // Give clients a moment to bind the seat's new keyboard and pointer.
        std::thread::sleep(Duration::from_millis(300));
        Ok(Self { c, pointer, keyboard, size, started: Instant::now() })
    }

    fn now(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn flush(&mut self) -> Result<()> {
        self.c.conn.flush().map_err(|e| Error::Protocol(format!("wayland: {e}")))
    }

    fn tap(&mut self, evdev: u32) -> Result<()> {
        for state in [1, 0] {
            self.keyboard.key(self.now(), evdev, state);
            self.flush()?;
            std::thread::sleep(KEY_GAP);
        }
        Ok(())
    }

    /// Perform input steps in order. Points outside the output are refused
    /// before anything is sent.
    pub fn run(&mut self, steps: &[Input]) -> Result<()> {
        let size = self.size;
        for step in steps {
            if let Input::Move { x, y } = step
                && !(x.is_finite() && y.is_finite() && *x >= 0.0 && *y >= 0.0 && *x < size.0 as f64 && *y < size.1 as f64)
            {
                return Err(Error::InvalidInput(format!("point ({x}, {y}) is outside the {}x{} desktop", size.0, size.1)));
            }
            if let Input::Key(chord) = step {
                parse_chord(chord)?;
            }
        }
        for step in steps {
            match step {
                Input::Move { x, y } => {
                    self.pointer.motion_absolute(self.now(), *x as u32, *y as u32, size.0, size.1);
                    self.pointer.frame();
                }
                Input::Button { button, pressed } => {
                    use wayland_client::protocol::wl_pointer::ButtonState;
                    let state = if *pressed { ButtonState::Pressed } else { ButtonState::Released };
                    self.pointer.button(self.now(), button.code(), state);
                    self.pointer.frame();
                }
                Input::Scroll { dx, dy } => {
                    use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
                    self.pointer.axis_source(AxisSource::Wheel);
                    for (axis, n) in [(Axis::VerticalScroll, *dy), (Axis::HorizontalScroll, *dx)] {
                        if n != 0 {
                            self.pointer.axis_discrete(self.now(), axis, 15.0 * n as f64, n);
                        }
                    }
                    self.pointer.frame();
                }
                Input::Key(chord) => {
                    let (mods, key) = parse_chord(chord)?;
                    let code = named_code(&key).unwrap_or(CHAR_CODES[0]);
                    upload_keymap(&self.keyboard, &[(code, key)])?;
                    self.c.roundtrip()?;
                    self.keyboard.modifiers(mods, 0, 0, 0);
                    self.tap(code)?;
                    self.keyboard.modifiers(0, 0, 0, 0);
                }
                Input::Text(text) => {
                    for chunk in keysym_chunks(text) {
                        let mut map: Vec<(u32, String)> = Vec::new();
                        let mut next = CHAR_CODES.iter();
                        let mut codes = Vec::with_capacity(chunk.len());
                        for s in chunk {
                            let code = match map.iter().find(|(_, k)| *k == s) {
                                Some((c, _)) => *c,
                                None => {
                                    let c = match named_code(&s) {
                                        Some(c) => c,
                                        None => *next.next().expect("chunk fits the character codes"),
                                    };
                                    map.push((c, s));
                                    c
                                }
                            };
                            codes.push(code);
                        }
                        upload_keymap(&self.keyboard, &map)?;
                        self.c.roundtrip()?;
                        for code in codes {
                            self.tap(code)?;
                        }
                    }
                }
                Input::Wait(d) => std::thread::sleep((*d).min(Duration::from_secs(10))),
            }
            self.flush()?;
        }
        self.c.roundtrip()
    }
}

/// Split text into keysym runs that each need at most [`KEYMAP_SLOTS`] distinct keysyms.
fn keysym_chunks(text: &str) -> Vec<Vec<String>> {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut distinct = std::collections::HashSet::new();
    for c in text.chars() {
        let s = char_keysym(c);
        let needs_slot = named_code(&s).is_none() && !distinct.contains(&s);
        if chunks.is_empty() || (needs_slot && distinct.len() == KEYMAP_SLOTS) {
            chunks.push(Vec::new());
            distinct.clear();
        }
        if named_code(&s).is_none() {
            distinct.insert(s.clone());
        }
        chunks.last_mut().expect("pushed above").push(s);
    }
    chunks
}

/// Upload a keymap in which each evdev code produces its keysym.
fn upload_keymap(kb: &ZwpVirtualKeyboardV1, keysyms: &[(u32, String)]) -> Result<()> {
    let text = keymap_text(keysyms);
    let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC).map_err(|e| Error::Io(e.into()))?;
    let file = std::fs::File::from(fd);
    file.write_all_at(text.as_bytes(), 0)?;
    file.write_all_at(&[0], text.len() as u64)?;
    kb.keymap(1, file.as_fd(), text.len() as u32 + 1);
    Ok(())
}

fn keymap_text(keysyms: &[(u32, String)]) -> String {
    let mut codes = String::new();
    let mut syms = String::new();
    for (code, s) in keysyms {
        codes.push_str(&format!("<K{code}> = {};\n", code + 8));
        syms.push_str(&format!("key <K{code}> {{ [ {s} ] }};\n"));
    }
    format!(
        "xkb_keymap {{\nxkb_keycodes \"session\" {{ minimum = 8; maximum = 255;\n{codes}}};\n\
         xkb_types \"session\" {{ include \"complete\" }};\n\
         xkb_compatibility \"session\" {{ include \"complete\" }};\n\
         xkb_symbols \"session\" {{\n{syms}}};\n}};\n"
    )
}

/// Real evdev code for keysyms whose physical key clients act on.
fn named_code(keysym: &str) -> Option<u32> {
    Some(match keysym {
        "Escape" => 1,
        "BackSpace" => 14,
        "Tab" => 15,
        "Return" => 28,
        "space" => 57,
        "Home" => 102,
        "Up" => 103,
        "Prior" => 104,
        "Left" => 105,
        "Right" => 106,
        "End" => 107,
        "Down" => 108,
        "Next" => 109,
        "Insert" => 110,
        "Delete" => 111,
        "F11" => 87,
        "F12" => 88,
        f if f.len() <= 3 && f.starts_with('F') => match f[1..].parse::<u32>() {
            Ok(n @ 1..=10) => 58 + n,
            _ => return None,
        },
        _ => return None,
    })
}

fn char_keysym(c: char) -> String {
    match c {
        '\n' | '\r' => "Return".into(),
        '\t' => "Tab".into(),
        c => format!("U{:04X}", c as u32),
    }
}

/// Parse `ctrl+shift+t` into a modifier mask and one keysym name.
fn parse_chord(chord: &str) -> Result<(u32, String)> {
    let parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let (key, mods) = parts.split_last().filter(|(k, _)| !k.is_empty()).ok_or_else(|| Error::InvalidInput("empty key chord".into()))?;
    let mut mask = 0;
    for m in mods {
        mask |= match m.to_ascii_lowercase().as_str() {
            "shift" => 1,
            "ctrl" | "control" => 4,
            "alt" => 8,
            "super" | "meta" | "cmd" => 64,
            other => return Err(Error::InvalidInput(format!("unknown modifier {other:?}"))),
        };
    }
    let lower = key.to_ascii_lowercase();
    let name = match lower.as_str() {
        "enter" | "return" => "Return".to_string(),
        "esc" | "escape" => "Escape".into(),
        "tab" => "Tab".into(),
        "space" => "space".into(),
        "backspace" => "BackSpace".into(),
        "delete" | "del" => "Delete".into(),
        "up" | "down" | "left" | "right" | "home" | "end" => {
            let mut s = lower.clone();
            s[..1].make_ascii_uppercase();
            s
        }
        "pageup" => "Prior".into(),
        "pagedown" => "Next".into(),
        _ if key.chars().count() == 1 => {
            let c = key.chars().next().expect("one char");
            if c.is_ascii_alphanumeric() { c.to_ascii_lowercase().to_string() } else { char_keysym(c) }
        }
        _ if key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => key.to_string(),
        _ => return Err(Error::InvalidInput(format!("key {key:?} is not a keysym name"))),
    };
    Ok((mask, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_into_masks_and_safe_keysyms() {
        assert_eq!(parse_chord("ctrl+l").unwrap(), (4, "l".into()));
        assert_eq!(parse_chord("Ctrl+Shift+T").unwrap(), (5, "t".into()));
        assert_eq!(parse_chord("enter").unwrap(), (0, "Return".into()));
        assert_eq!(parse_chord("F5").unwrap(), (0, "F5".into()));
        assert_eq!(parse_chord("pagedown").unwrap(), (0, "Next".into()));
        assert_eq!(parse_chord("alt+left").unwrap(), (8, "Left".into()));
        assert_eq!(parse_chord("/").unwrap(), (0, "U002F".into()));
        assert!(parse_chord("hyper+x").is_err());
        assert!(parse_chord("").is_err());
        assert!(parse_chord("a ] }; key <X> { [ b").is_err(), "no keymap injection");
    }

    #[test]
    fn keymap_maps_evdev_codes_to_keysyms_in_order() {
        let km = keymap_text(&[(2, "U00E9".into()), (28, "Return".into())]);
        assert!(km.contains("<K2> = 10;") && km.contains("key <K2> { [ U00E9 ] };"));
        assert!(km.contains("<K28> = 36;") && km.contains("key <K28> { [ Return ] };"));
        assert_eq!(named_code("Return"), Some(28));
        assert_eq!(named_code("F5"), Some(63));
        assert_eq!(named_code("U003F"), None);
        assert!(!CHAR_CODES.contains(&14) && !CHAR_CODES.contains(&28) && !CHAR_CODES.contains(&1));
        assert_eq!(char_keysym('é'), "U00E9");
        assert_eq!(char_keysym('\n'), "Return");
    }

    #[test]
    fn frames_convert_from_both_byte_orders_and_honour_y_invert() {
        // 2x2, stride 12 (padding), XRGB8888: memory B,G,R,X.
        let mut raw = vec![0u8; 24];
        raw[0..4].copy_from_slice(&[3, 2, 1, 0]); // (0,0) = rgb(1,2,3)
        raw[12..16].copy_from_slice(&[6, 5, 4, 0]); // (0,1) = rgb(4,5,6)
        let f = to_rgba(&raw, wl_shm::Format::Xrgb8888, 2, 2, 12, false).unwrap();
        assert_eq!(&f.rgba[0..4], &[1, 2, 3, 255]);
        assert_eq!(&f.rgba[8..12], &[4, 5, 6, 255]);
        let inv = to_rgba(&raw, wl_shm::Format::Xrgb8888, 2, 2, 12, true).unwrap();
        assert_eq!(&inv.rgba[0..4], &[4, 5, 6, 255]);
        let abgr = to_rgba(&raw, wl_shm::Format::Xbgr8888, 2, 2, 12, false).unwrap();
        assert_eq!(&abgr.rgba[0..4], &[3, 2, 1, 255]);
        assert!(to_rgba(&raw, wl_shm::Format::Rgb565, 2, 2, 12, false).is_err());
        let png = f.to_png().unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn long_text_splits_into_chunks_within_the_keymap_budget() {
        let text: String = (0..500u32).filter_map(|i| char::from_u32(0x4e00 + i)).collect();
        let chunks = keysym_chunks(&text);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 500);
        assert!(chunks.iter().all(|c| c.iter().collect::<std::collections::HashSet<_>>().len() <= KEYMAP_SLOTS));
        assert_eq!(keysym_chunks("aab").len(), 1);
    }
}
