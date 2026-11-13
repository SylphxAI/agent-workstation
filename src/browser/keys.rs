//! Key chords for in-page keyboard events (`Input.dispatchKeyEvent`).
//!
//! Web operations deliver keys to the active tab's page, not to the desktop:
//! they reach the page's focused element whatever has native keyboard focus
//! (the address bar, a bubble), and browser shortcuts such as `ctrl+l` never
//! fire. Desktop keys go through [`crate::Input`] instead.

use crate::error::{Error, Result};

/// CDP modifier bits.
pub(crate) const ALT: i64 = 1;
pub(crate) const CTRL: i64 = 2;
pub(crate) const META: i64 = 4;
pub(crate) const SHIFT: i64 = 8;

/// One key as the page sees it (US layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    /// `KeyboardEvent.key`.
    pub key: String,
    /// `KeyboardEvent.code`.
    pub code: String,
    /// Windows virtual key code (`keyCode`).
    pub vk: i64,
    /// Text the key inserts without modifiers.
    pub text: Option<String>,
    /// The CDP modifier bit when this key is a modifier.
    pub modifier: i64,
}

impl Key {
    /// The text this key inserts with `modifiers` held: none while Ctrl,
    /// Alt or Meta is held (the key is a shortcut, not input).
    pub fn text_with(&self, modifiers: i64) -> Option<String> {
        if modifiers & (CTRL | ALT | META) != 0 {
            return None;
        }
        let text = self.text.as_deref()?;
        Some(if modifiers & SHIFT != 0 {
            shifted(text)
        } else {
            text.to_string()
        })
    }

    /// `KeyboardEvent.key` with `modifiers` held.
    pub fn key_with(&self, modifiers: i64) -> String {
        match self.text_with(modifiers & SHIFT) {
            Some(t) if self.text.is_some() && t != "\r" => t,
            _ => self.key.clone(),
        }
    }
}

fn shifted(text: &str) -> String {
    const PAIRS: &[(char, char)] = &[
        ('1', '!'),
        ('2', '@'),
        ('3', '#'),
        ('4', '$'),
        ('5', '%'),
        ('6', '^'),
        ('7', '&'),
        ('8', '*'),
        ('9', '('),
        ('0', ')'),
        ('-', '_'),
        ('=', '+'),
        ('[', '{'),
        (']', '}'),
        ('\\', '|'),
        (';', ':'),
        ('\'', '"'),
        (',', '<'),
        ('.', '>'),
        ('/', '?'),
        ('`', '~'),
    ];
    text.chars()
        .map(|c| {
            PAIRS
                .iter()
                .find(|(a, _)| *a == c)
                .map(|(_, b)| *b)
                .unwrap_or_else(|| c.to_ascii_uppercase())
        })
        .collect()
}

fn named(key: &str, code: &str, vk: i64, text: Option<&str>, modifier: i64) -> Key {
    Key {
        key: key.into(),
        code: code.into(),
        vk,
        text: text.map(String::from),
        modifier,
    }
}

/// One key by name: `Enter`, `ArrowUp`, `F5`, `shift`, `a`, `7`, `/`, `é`.
pub(crate) fn key(name: &str) -> Result<Key> {
    let lower = name.trim().to_ascii_lowercase();
    let k = match lower.as_str() {
        "shift" => named("Shift", "ShiftLeft", 16, None, SHIFT),
        "ctrl" | "control" | "ctl" => named("Control", "ControlLeft", 17, None, CTRL),
        "alt" | "option" => named("Alt", "AltLeft", 18, None, ALT),
        "meta" | "super" | "cmd" | "command" | "win" | "windows" => {
            named("Meta", "MetaLeft", 91, None, META)
        }
        "enter" | "return" => named("Enter", "Enter", 13, Some("\r"), 0),
        "tab" => named("Tab", "Tab", 9, None, 0),
        "escape" | "esc" => named("Escape", "Escape", 27, None, 0),
        "backspace" => named("Backspace", "Backspace", 8, None, 0),
        "delete" | "del" => named("Delete", "Delete", 46, None, 0),
        "insert" => named("Insert", "Insert", 45, None, 0),
        "space" | "spacebar" => named(" ", "Space", 32, Some(" "), 0),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, None, 0),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, None, 0),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, None, 0),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, None, 0),
        "home" => named("Home", "Home", 36, None, 0),
        "end" => named("End", "End", 35, None, 0),
        "pageup" | "prior" => named("PageUp", "PageUp", 33, None, 0),
        "pagedown" | "next" => named("PageDown", "PageDown", 34, None, 0),
        f if f.len() <= 3 && f.starts_with('f') && f[1..].parse::<i64>().is_ok() => {
            let n: i64 = f[1..].parse().expect("checked");
            if !(1..=12).contains(&n) {
                return Err(Error::InvalidInput(format!(
                    "key {name:?} is not a key name"
                )));
            }
            let label = format!("F{n}");
            named(&label, &label, 111 + n, None, 0)
        }
        _ => {
            let mut chars = name.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return Err(Error::InvalidInput(format!(
                    "key {name:?} is not a key name"
                )));
            };
            char_key(c)
        }
    };
    Ok(k)
}

/// A printable character on the US layout; other characters are sent as
/// text with no physical key.
fn char_key(c: char) -> Key {
    let lower = c.to_ascii_lowercase();
    let (code, vk) = match lower {
        'a'..='z' => (
            format!("Key{}", lower.to_ascii_uppercase()),
            lower.to_ascii_uppercase() as i64,
        ),
        '0'..='9' => (format!("Digit{lower}"), lower as i64),
        _ => match PUNCTUATION.iter().find(|(p, _, _)| *p == c) {
            Some((_, code, vk)) => ((*code).to_string(), *vk),
            None => (String::new(), 0),
        },
    };
    let text = c.to_string();
    Key {
        key: text.clone(),
        code,
        vk,
        text: Some(text),
        modifier: 0,
    }
}

const PUNCTUATION: &[(char, &str, i64)] = &[
    ('-', "Minus", 189),
    ('=', "Equal", 187),
    ('[', "BracketLeft", 219),
    (']', "BracketRight", 221),
    ('\\', "Backslash", 220),
    (';', "Semicolon", 186),
    ('\'', "Quote", 222),
    (',', "Comma", 188),
    ('.', "Period", 190),
    ('/', "Slash", 191),
    ('`', "Backquote", 192),
];

/// `ctrl+shift+t` → modifier keys (in order) and the final key. A chord of
/// modifiers only (`shift`) has no final key beyond the modifier itself.
pub(crate) fn chord(raw: &str) -> Result<Vec<Key>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::InvalidInput("empty key chord".into()));
    }
    // A lone "+" (or a chord ending in "++") names the plus key.
    let parts: Vec<&str> = if raw == "+" {
        vec!["+"]
    } else if let Some(head) = raw.strip_suffix("++") {
        head.split('+').chain(std::iter::once("+")).collect()
    } else {
        raw.split('+').collect()
    };
    let mut keys = Vec::with_capacity(parts.len());
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate() {
        let k = key(part)?;
        if i < last && k.modifier == 0 {
            return Err(Error::InvalidInput(format!(
                "{part:?} in {raw:?} is not a modifier; only the last key of a chord may be another key"
            )));
        }
        keys.push(k);
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_name_modifiers_then_one_key() {
        let k = chord("ctrl+shift+T").unwrap();
        assert_eq!(k.len(), 3);
        assert_eq!((k[0].modifier, k[1].modifier), (CTRL, SHIFT));
        assert_eq!((k[2].code.as_str(), k[2].vk), ("KeyT", 84));
        assert_eq!(
            k[2].text_with(CTRL | SHIFT),
            None,
            "shortcuts insert no text"
        );
        assert_eq!(
            chord("Enter").unwrap()[0].text_with(0).as_deref(),
            Some("\r")
        );
        assert_eq!(chord("ArrowUp").unwrap()[0].vk, 38);
        assert_eq!(chord("F5").unwrap()[0].vk, 116);
        assert_eq!(chord("shift").unwrap()[0].modifier, SHIFT);
        assert_eq!(chord("+").unwrap()[0].key, "+");
        assert_eq!(chord("ctrl++").unwrap()[1].key, "+");
        assert!(chord("w+d").is_err(), "two non-modifier keys");
        assert!(chord("").is_err());
        assert!(chord("F13").is_err());
        assert!(chord("hyper+x").is_err());
    }

    #[test]
    fn shift_changes_text_and_key() {
        let a = key("a").unwrap();
        assert_eq!(a.text_with(SHIFT).as_deref(), Some("A"));
        assert_eq!(a.key_with(SHIFT), "A");
        let one = key("1").unwrap();
        assert_eq!(one.text_with(SHIFT).as_deref(), Some("!"));
        assert_eq!((one.code.as_str(), one.vk), ("Digit1", 49));
        let e = key("é").unwrap();
        assert_eq!((e.text_with(0).as_deref(), e.vk), (Some("é"), 0));
        assert_eq!(key("Enter").unwrap().key_with(0), "Enter");
    }
}
