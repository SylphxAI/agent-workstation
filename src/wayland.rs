//! Desktop operations through the Wayland output.
//!
//! - Screenshots: `wlr-screencopy` into a `wl_shm` buffer backed by a memfd,
//!   read back with `pread` (no shared mapping, no unsafe code).
//! - Pointer: `wlr-virtual-pointer` with absolute motion in output pixels.
//! - Keyboard: `virtual-keyboard` with a keymap generated per call that maps
//!   exactly the keysyms the call needs, so any Unicode text types without
//!   depending on the desktop's layout.
//!
//! - Clipboard: `wlr-data-control`. Reading takes the current selection
//!   over a short-lived connection; setting it starts an owner thread that
//!   serves pastes until another client replaces the selection or the
//!   compositor exits.
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
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::error::{Error, Result};

const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Evdev codes of character keys (number row and letter rows). Text is typed
/// on these codes only: clients treat codes such as Backspace or Enter by
/// their physical meaning whatever keysym they carry.
const CHAR_CODES: [u32; 47] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53,
];
/// Distinct character keysyms one generated keymap carries.
#[cfg(test)]
const KEYMAP_SLOTS: usize = CHAR_CODES.len();
const KEY_GAP: Duration = Duration::from_millis(4);

pub use crate::desktop::{Button, Frame, Input};

impl Button {
    fn code(self) -> u32 {
        match self {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
            Button::Back => 0x113,
            Button::Forward => 0x114,
        }
    }
}

#[derive(Default)]
struct State {
    buffer: Option<(wl_shm::Format, u32, u32, u32)>,
    buffer_done: bool,
    flags_y_invert: bool,
    outcome: Option<bool>,
    /// The clipboard selection offered to this client.
    selection: Option<ZwlrDataControlOfferV1>,
    /// Text this client serves as the clipboard owner.
    owned: Option<String>,
    /// The owned selection was replaced.
    cancelled: bool,
}

/// MIME types offered for clipboard text, most preferred first.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
/// Largest clipboard text read.
const MAX_CLIPBOARD: usize = 16 << 20;
/// How long a clipboard owner may take to start sending.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(5);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        ev: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event as E;
        match ev {
            E::Buffer {
                format: WEnum::Value(f),
                width,
                height,
                stride,
            } if s.buffer.is_none() => {
                s.buffer = Some((f, width, height, stride));
            }
            E::BufferDone => s.buffer_done = true,
            E::Flags {
                flags: WEnum::Value(f),
            } => {
                s.flags_y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            E::Ready { .. } => s.outcome = Some(true),
            E::Failed => s.outcome = Some(false),
            _ => {}
        }
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        ev: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event as E;
        match ev {
            E::Selection { id } => {
                if let Some(old) = std::mem::replace(&mut s.selection, id) {
                    old.destroy();
                }
            }
            E::PrimarySelection { id: Some(offer) } => offer.destroy(),
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, std::sync::Mutex::new(Vec::new())),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, std::sync::Mutex<Vec<String>>> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        ev: zwlr_data_control_offer_v1::Event,
        mimes: &std::sync::Mutex<Vec<String>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let zwlr_data_control_offer_v1::Event::Offer { mime_type } = ev else {
            return;
        };
        if let Ok(mut m) = mimes.lock() {
            m.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        s: &mut Self,
        source: &ZwlrDataControlSourceV1,
        ev: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_source_v1::Event as E;
        match ev {
            E::Send { fd, .. } => {
                if let Some(text) = &s.owned {
                    // A reader that closes early only ends its own paste.
                    let _ =
                        std::io::Write::write_all(&mut std::fs::File::from(fd), text.as_bytes());
                }
            }
            E::Cancelled => {
                s.cancelled = true;
                s.owned = None;
                source.destroy();
            }
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
delegate_noop!(State: ZwlrDataControlManagerV1);

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
        let conn = Connection::from_socket(stream)
            .map_err(|e| Error::Protocol(format!("wayland: {e}")))?;
        let (globals, queue) = registry_queue_init::<State>(&conn)
            .map_err(|e| Error::Protocol(format!("wayland registry: {e}")))?;
        Ok(Self {
            conn,
            queue,
            globals,
            state: State::default(),
        })
    }

    fn bind<I>(&self, versions: std::ops::RangeInclusive<u32>) -> Result<I>
    where
        I: Proxy + 'static,
        State: Dispatch<I, ()>,
    {
        self.globals
            .bind::<I, State, ()>(&self.queue.handle(), versions, ())
            .map_err(|e| {
                Error::Unsupported(format!("compositor lacks {}: {e}", I::interface().name))
            })
    }

    fn roundtrip(&mut self) -> Result<()> {
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| Error::Protocol(format!("wayland: {e}")))?;
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
            return Err(Error::Protocol(
                "screencopy offered no shared-memory buffer".into(),
            ));
        }
        c.roundtrip()?;
    }
    let (format, width, height, stride) = c.state.buffer.expect("checked above");
    let size = stride as usize * height as usize;
    let fd = rustix::fs::memfd_create("desktop-frame", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| Error::Io(e.into()))?;
    rustix::fs::ftruncate(&fd, size as u64).map_err(|e| Error::Io(e.into()))?;
    let pool = shm.create_pool(fd.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        &qh,
        (),
    );
    frame.copy(&buffer);
    while c.state.outcome.is_none() {
        if Instant::now() > deadline {
            return Err(Error::Timeout("screencopy frame".into()));
        }
        c.queue
            .blocking_dispatch(&mut c.state)
            .map_err(|e| Error::Protocol(format!("wayland: {e}")))?;
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

/// The clipboard's text, or `None` when it holds no text.
pub fn clipboard_text(socket: &Path) -> Result<Option<String>> {
    let mut c = Client::connect(socket)?;
    let qh = c.queue.handle();
    let seat: wl_seat::WlSeat = c.bind(1..=7)?;
    let manager: ZwlrDataControlManagerV1 = c.bind(1..=2)?;
    let device = manager.get_data_device(&seat, &qh, ());
    // The compositor answers a new device with the current selection.
    c.roundtrip()?;
    c.roundtrip()?;
    let Some(offer) = c.state.selection.take() else {
        device.destroy();
        return Ok(None);
    };
    let offered = offer
        .data::<std::sync::Mutex<Vec<String>>>()
        .and_then(|m| m.lock().ok().map(|m| m.clone()))
        .unwrap_or_default();
    let Some(mime) = TEXT_MIMES.iter().find(|m| offered.iter().any(|o| o == *m)) else {
        offer.destroy();
        device.destroy();
        return Ok(None);
    };
    let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| Error::Io(e.into()))?;
    offer.receive(mime.to_string(), write.as_fd());
    c.roundtrip()?;
    drop(write);
    let mut out = Vec::new();
    let mut buf = [0u8; 64 << 10];
    let file = std::fs::File::from(read);
    loop {
        let mut fds = [rustix::event::PollFd::new(
            &file,
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: CLIPBOARD_TIMEOUT.as_secs() as _,
            tv_nsec: 0,
        };
        if rustix::event::poll(&mut fds, Some(&timeout)).map_err(|e| Error::Io(e.into()))? == 0 {
            return Err(Error::Timeout("clipboard owner did not send".into()));
        }
        let n = std::io::Read::read(&mut &file, &mut buf)?;
        if n == 0 {
            break;
        }
        if out.len() + n > MAX_CLIPBOARD {
            return Err(Error::InvalidInput(format!(
                "clipboard text exceeds {} MiB",
                MAX_CLIPBOARD >> 20
            )));
        }
        out.extend_from_slice(&buf[..n]);
    }
    offer.destroy();
    device.destroy();
    String::from_utf8(out)
        .map(Some)
        .map_err(|_| Error::InvalidInput("clipboard text is not UTF-8".into()))
}

/// Make `text` the clipboard. Returns once the compositor has the new
/// selection; a thread then serves pastes until another client replaces it
/// or the compositor exits.
pub fn set_clipboard_text(socket: &Path, text: String) -> Result<()> {
    if text.len() > MAX_CLIPBOARD {
        return Err(Error::InvalidInput(format!(
            "clipboard text exceeds {} MiB",
            MAX_CLIPBOARD >> 20
        )));
    }
    let mut c = Client::connect(socket)?;
    let qh = c.queue.handle();
    let seat: wl_seat::WlSeat = c.bind(1..=7)?;
    let manager: ZwlrDataControlManagerV1 = c.bind(1..=2)?;
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for mime in TEXT_MIMES {
        source.offer(mime.to_string());
    }
    c.state.owned = Some(text);
    device.set_selection(Some(&source));
    c.roundtrip()?;
    std::thread::Builder::new()
        .name("desktop-clipboard".into())
        .spawn(move || {
            while !c.state.cancelled {
                if c.queue.blocking_dispatch(&mut c.state).is_err() {
                    break;
                }
            }
            device.destroy();
            let _ = c.conn.flush();
        })
        .map_err(|e| Error::Process(format!("clipboard owner: {e}")))?;
    Ok(())
}

fn to_rgba(
    raw: &[u8],
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
) -> Result<Frame> {
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
            let (r, g, b) = if bgr {
                (px[2], px[1], px[0])
            } else {
                (px[0], px[1], px[2])
            };
            rgba.extend_from_slice(&[r, g, b, 255]);
        }
    }
    Ok(Frame {
        width,
        height,
        rgba,
    })
}

/// A virtual pointer and keyboard held for the life of a session, so the
/// desktop's seat always has both and focused clients stay bound to them.
pub struct InputDevice {
    c: Client,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    size: (u32, u32),
    started: Instant,
    /// Modifier mask of the held keys.
    mods: u32,
    /// Keys held by [`Input::KeyDown`]: evdev code, keysym, modifier bit.
    held: Vec<(u32, String, u32)>,
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
        let pointer = c
            .bind::<ZwlrVirtualPointerManagerV1>(1..=2)?
            .create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = c
            .bind::<ZwpVirtualKeyboardManagerV1>(1..=1)?
            .create_virtual_keyboard(&seat, &qh, ());
        upload_keymap(&keyboard, &[(1, "Escape".to_string())])?;
        c.roundtrip()?;
        Ok(Self {
            c,
            pointer,
            keyboard,
            size,
            started: Instant::now(),
            mods: 0,
            held: Vec::new(),
        })
    }

    fn now(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn flush(&mut self) -> Result<()> {
        self.c
            .conn
            .flush()
            .map_err(|e| Error::Protocol(format!("wayland: {e}")))
    }

    fn key(&mut self, evdev: u32, pressed: bool) -> Result<()> {
        self.keyboard.key(self.now(), evdev, u32::from(pressed));
        self.flush()?;
        std::thread::sleep(KEY_GAP);
        Ok(())
    }

    fn tap(&mut self, evdev: u32) -> Result<()> {
        self.key(evdev, true)?;
        self.key(evdev, false)
    }

    /// Character codes no held key occupies.
    fn free_codes(&self) -> Vec<u32> {
        CHAR_CODES
            .iter()
            .copied()
            .filter(|c| !self.held.iter().any(|(h, _, _)| h == c))
            .collect()
    }

    /// Upload a keymap with every held key plus `extra`.
    fn keymap(&mut self, extra: &[(u32, String)]) -> Result<()> {
        let mut map: Vec<(u32, String)> =
            self.held.iter().map(|(c, k, _)| (*c, k.clone())).collect();
        for (code, keysym) in extra {
            if !map.iter().any(|(c, _)| c == code) {
                map.push((*code, keysym.clone()));
            }
        }
        upload_keymap(&self.keyboard, &map)?;
        self.c.roundtrip()
    }

    fn code_for(&self, keysym: &str) -> Result<u32> {
        match named_code(keysym) {
            Some(c) => Ok(c),
            None => self
                .free_codes()
                .first()
                .copied()
                .ok_or_else(|| Error::InvalidInput("too many keys held".into())),
        }
    }

    /// Perform input steps in order. Points outside the output are refused
    /// before anything is sent. Keys held by [`Input::KeyDown`] are released
    /// when the steps end, whether or not they succeed.
    pub fn run(&mut self, steps: &[Input]) -> Result<()> {
        let size = self.size;
        for step in steps {
            match step {
                Input::Move { x, y }
                    if !(x.is_finite()
                        && y.is_finite()
                        && *x >= 0.0
                        && *y >= 0.0
                        && *x < size.0 as f64
                        && *y < size.1 as f64) =>
                {
                    return Err(Error::InvalidInput(format!(
                        "point ({x}, {y}) is outside the {}x{} desktop",
                        size.0, size.1
                    )));
                }
                Input::Key(chord) | Input::KeyDown(chord) | Input::KeyUp(chord) => {
                    parse_chord(chord)?;
                }
                _ => {}
            }
        }
        let result = steps.iter().try_for_each(|step| self.step(step));
        let released = self.release_held();
        result.and(released)?;
        self.c.roundtrip()
    }

    fn step(&mut self, step: &Input) -> Result<()> {
        let size = self.size;
        match step {
            Input::Move { x, y } => {
                self.pointer
                    .motion_absolute(self.now(), *x as u32, *y as u32, size.0, size.1);
                self.pointer.frame();
            }
            Input::Button { button, pressed } => {
                use wayland_client::protocol::wl_pointer::ButtonState;
                let state = if *pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                self.pointer.button(self.now(), button.code(), state);
                self.pointer.frame();
            }
            Input::Scroll { dx, dy } => {
                use wayland_client::protocol::wl_pointer::{Axis, AxisSource};
                self.pointer.axis_source(AxisSource::Wheel);
                for (axis, n) in [(Axis::VerticalScroll, *dy), (Axis::HorizontalScroll, *dx)] {
                    if n != 0 {
                        self.pointer
                            .axis_discrete(self.now(), axis, 15.0 * n as f64, n);
                    }
                }
                self.pointer.frame();
            }
            Input::Key(chord) => {
                let keys = parse_chord(chord)?;
                let (last, prefix) = keys.split_last().expect("a chord has a key");
                let mods = prefix.iter().fold(self.mods, |m, k| m | k.1);
                let code = self.code_for(&last.0)?;
                self.keymap(&[(code, last.0.clone())])?;
                self.keyboard.modifiers(mods, 0, 0, 0);
                self.tap(code)?;
                self.keyboard.modifiers(self.mods, 0, 0, 0);
            }
            Input::KeyDown(chord) => {
                for (keysym, bit) in parse_chord(chord)? {
                    if self.held.iter().any(|(_, k, _)| *k == keysym) {
                        continue;
                    }
                    let code = self.code_for(&keysym)?;
                    self.held.push((code, keysym, bit));
                    self.keymap(&[])?;
                    self.mods |= bit;
                    self.keyboard.modifiers(self.mods, 0, 0, 0);
                    self.key(code, true)?;
                }
            }
            Input::KeyUp(chord) => {
                for (keysym, _) in parse_chord(chord)?.into_iter().rev() {
                    self.release(&keysym)?;
                }
            }
            Input::Text(text) => {
                let free = self.free_codes();
                for chunk in keysym_chunks(text, free.len()) {
                    let mut map: Vec<(u32, String)> = Vec::new();
                    let mut next = free.iter();
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
                    self.keymap(&map)?;
                    for code in codes {
                        self.tap(code)?;
                    }
                }
            }
            Input::Wait(d) => std::thread::sleep((*d).min(MAX_WAIT)),
        }
        self.flush()
    }

    fn release(&mut self, keysym: &str) -> Result<()> {
        let Some(i) = self.held.iter().position(|(_, k, _)| k == keysym) else {
            return Ok(());
        };
        let (code, _, _) = self.held.remove(i);
        self.mods = self.held.iter().fold(0, |m, (_, _, bit)| m | bit);
        self.key(code, false)?;
        self.keyboard.modifiers(self.mods, 0, 0, 0);
        self.flush()
    }

    fn release_held(&mut self) -> Result<()> {
        while let Some((_, keysym, _)) = self.held.last().cloned() {
            self.release(&keysym)?;
        }
        Ok(())
    }
}

/// Longest single [`Input::Wait`].
pub const MAX_WAIT: Duration = Duration::from_secs(10);

/// Split text into keysym runs that each need at most `slots` distinct
/// non-named keysyms.
fn keysym_chunks(text: &str, slots: usize) -> Vec<Vec<String>> {
    let slots = slots.max(1);
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut distinct = std::collections::HashSet::new();
    for c in text.chars() {
        let s = char_keysym(c);
        let needs_slot = named_code(&s).is_none() && !distinct.contains(&s);
        if chunks.is_empty() || (needs_slot && distinct.len() == slots) {
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
    let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| Error::Io(e.into()))?;
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
        "Control_L" => 29,
        "Return" => 28,
        "Shift_L" => 42,
        "Alt_L" => 56,
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
        "Super_L" => 125,
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

/// A modifier name's keysym and modifier bit.
fn modifier(name: &str) -> Option<(&'static str, u32)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "shift" => ("Shift_L", 1),
        "ctrl" | "control" => ("Control_L", 4),
        "alt" => ("Alt_L", 8),
        "super" | "meta" | "cmd" => ("Super_L", 64),
        _ => return None,
    })
}

/// Parse `ctrl+shift+t` into its keys in press order: each a keysym and
/// its modifier bit (0 for the final, non-modifier key). Every key but the
/// last must be a modifier; the last may be one too (`shift`).
fn parse_chord(chord: &str) -> Result<Vec<(String, u32)>> {
    let parts: Vec<&str> = chord.split('+').map(str::trim).collect();
    let (key, mods) = parts
        .split_last()
        .filter(|(k, _)| !k.is_empty())
        .ok_or_else(|| Error::InvalidInput("empty key chord".into()))?;
    let mut keys = Vec::with_capacity(parts.len());
    for m in mods {
        let (sym, bit) =
            modifier(m).ok_or_else(|| Error::InvalidInput(format!("unknown modifier {m:?}")))?;
        keys.push((sym.to_string(), bit));
    }
    if let Some((sym, bit)) = modifier(key) {
        keys.push((sym.to_string(), bit));
        return Ok(keys);
    }
    let lower = key.to_ascii_lowercase();
    let name = match lower.as_str() {
        "enter" | "return" => "Return".to_string(),
        "esc" | "escape" => "Escape".into(),
        "tab" => "Tab".into(),
        "space" => "space".into(),
        "backspace" => "BackSpace".into(),
        "delete" | "del" => "Delete".into(),
        "insert" => "Insert".into(),
        "up" | "down" | "left" | "right" | "home" | "end" => {
            let mut s = lower.clone();
            s[..1].make_ascii_uppercase();
            s
        }
        "pageup" => "Prior".into(),
        "pagedown" => "Next".into(),
        _ if key.chars().count() == 1 => {
            let c = key.chars().next().expect("one char");
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase().to_string()
            } else {
                char_keysym(c)
            }
        }
        f if f.len() <= 3
            && f.starts_with('f')
            && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) =>
        {
            f.to_ascii_uppercase()
        }
        // Other keysym names (`Print`, `XF86AudioMute`) pass through as is.
        _ if key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => key.to_string(),
        _ => {
            return Err(Error::InvalidInput(format!(
                "key {key:?} is not a key name"
            )));
        }
    };
    keys.push((name, 0));
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_into_masks_and_safe_keysyms() {
        let keys = |c: &str| parse_chord(c).unwrap();
        let k = |s: &str, bit: u32| (s.to_string(), bit);
        assert_eq!(keys("ctrl+l"), [k("Control_L", 4), k("l", 0)]);
        assert_eq!(
            keys("Ctrl+Shift+T"),
            [k("Control_L", 4), k("Shift_L", 1), k("t", 0)]
        );
        assert_eq!(keys("enter"), [k("Return", 0)]);
        assert_eq!(keys("F5"), [k("F5", 0)]);
        assert_eq!(keys("f12"), [k("F12", 0)]);
        assert_eq!(keys("pagedown"), [k("Next", 0)]);
        assert_eq!(keys("alt+left"), [k("Alt_L", 8), k("Left", 0)]);
        assert_eq!(keys("/"), [k("U002F", 0)]);
        assert_eq!(keys("shift"), [k("Shift_L", 1)], "a modifier alone");
        assert_eq!(keys("ctrl+shift"), [k("Control_L", 4), k("Shift_L", 1)]);
        assert!(parse_chord("hyper+x").is_err());
        assert_eq!(keys("Print"), [k("Print", 0)]);
        assert!(parse_chord("").is_err());
        assert!(
            parse_chord("a ] }; key <X> { [ b").is_err(),
            "no keymap injection"
        );
    }

    #[test]
    fn keymap_maps_evdev_codes_to_keysyms_in_order() {
        let km = keymap_text(&[(2, "U00E9".into()), (28, "Return".into())]);
        assert!(km.contains("<K2> = 10;") && km.contains("key <K2> { [ U00E9 ] };"));
        assert!(km.contains("<K28> = 36;") && km.contains("key <K28> { [ Return ] };"));
        assert_eq!(named_code("Return"), Some(28));
        for m in ["Shift_L", "Control_L", "Alt_L", "Super_L"] {
            assert!(!CHAR_CODES.contains(&named_code(m).unwrap()), "{m}");
        }
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
        let text: String = (0..500u32)
            .filter_map(|i| char::from_u32(0x4e00 + i))
            .collect();
        let chunks = keysym_chunks(&text, KEYMAP_SLOTS);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), 500);
        assert!(
            chunks
                .iter()
                .all(|c| c.iter().collect::<std::collections::HashSet<_>>().len() <= KEYMAP_SLOTS)
        );
        assert_eq!(keysym_chunks("aab", KEYMAP_SLOTS).len(), 1);
        assert_eq!(
            keysym_chunks("abc", 2).len(),
            2,
            "held keys shrink the budget"
        );
    }
}
