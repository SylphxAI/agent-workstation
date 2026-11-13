//! Portable desktop frames and input descriptions.

use std::time::Duration;

use crate::error::{Error, Result};

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
        let mut w = enc
            .write_header()
            .map_err(|e| Error::Protocol(format!("png: {e}")))?;
        w.write_image_data(&self.rgba)
            .map_err(|e| Error::Protocol(format!("png: {e}")))?;
        w.finish()
            .map_err(|e| Error::Protocol(format!("png: {e}")))?;
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
