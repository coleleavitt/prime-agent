//! Key chord parsing (`"cmd+shift+f"`, `"Return"`) and the macOS keycodes.
//!
//! Every backend parses chords here; each then translates the canonical
//! key name into its own vocabulary (macOS virtual keycodes, X11 keysyms,
//! the Wayland backend's xkb keysyms).

use std::collections::BTreeSet;

use serde_json::json;

use crate::error::{head, invalid, ComputerUseError};
use crate::pyfmt::repr_str;

/// One canonical modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    Cmd,
    Ctrl,
    Alt,
    Shift,
}

impl Modifier {
    fn parse(token: &str) -> Option<Self> {
        match token.to_lowercase().as_str() {
            "cmd" | "command" | "super" => Some(Modifier::Cmd),
            "ctrl" | "control" => Some(Modifier::Ctrl),
            "alt" | "option" | "opt" => Some(Modifier::Alt),
            "shift" => Some(Modifier::Shift),
            _ => None,
        }
    }
}

/// A parsed chord: canonical modifiers plus one canonical key name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedChord {
    pub modifiers: BTreeSet<Modifier>,
    /// A named key (`Return`, `F5`, `PageUp`, ...) or one lowercase character.
    pub key: String,
}

const NAMED_KEYS: &[(&str, &str)] = &[
    ("return", "Return"),
    ("enter", "Return"),
    ("tab", "Tab"),
    ("escape", "Escape"),
    ("space", "Space"),
    ("delete", "Delete"),
    ("backspace", "Delete"),
    ("forwarddelete", "ForwardDelete"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "PageUp"),
    ("pagedown", "PageDown"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
];

/// The standard macOS virtual keycodes, keyed by canonical key name.
pub const KEYCODES: &[(&str, u16)] = &[
    ("Return", 36),
    ("Enter", 36),
    ("Tab", 48),
    ("Space", 49),
    ("Delete", 51),
    ("Backspace", 51),
    ("Escape", 53),
    ("Home", 115),
    ("PageUp", 116),
    ("ForwardDelete", 117),
    ("End", 119),
    ("PageDown", 121),
    ("Left", 123),
    ("Right", 124),
    ("Down", 125),
    ("Up", 126),
    ("a", 0),
    ("s", 1),
    ("d", 2),
    ("f", 3),
    ("h", 4),
    ("g", 5),
    ("z", 6),
    ("x", 7),
    ("c", 8),
    ("v", 9),
    ("b", 11),
    ("q", 12),
    ("w", 13),
    ("e", 14),
    ("r", 15),
    ("y", 16),
    ("t", 17),
    ("o", 31),
    ("u", 32),
    ("i", 34),
    ("p", 35),
    ("l", 37),
    ("j", 38),
    ("k", 40),
    ("n", 45),
    ("m", 46),
    ("1", 18),
    ("2", 19),
    ("3", 20),
    ("4", 21),
    ("6", 22),
    ("5", 23),
    ("9", 25),
    ("7", 26),
    ("8", 28),
    ("0", 29),
    ("=", 24),
    ("-", 27),
    ("]", 30),
    ("[", 33),
    (";", 41),
    ("\\", 42),
    (",", 43),
    ("/", 44),
    (".", 47),
    ("`", 50),
    ("'", 39),
    (" ", 49),
    ("F1", 122),
    ("F2", 120),
    ("F3", 99),
    ("F4", 118),
    ("F5", 96),
    ("F6", 97),
    ("F7", 98),
    ("F8", 100),
    ("F9", 101),
    ("F10", 109),
    ("F11", 103),
    ("F12", 111),
];

/// The macOS virtual keycode of one canonical key name.
#[must_use]
pub fn keycode(key: &str) -> Option<u16> {
    KEYCODES
        .iter()
        .find(|(name, _)| *name == key)
        .map(|&(_, code)| code)
}

const SUPPORTED_MODIFIERS: &str = "cmd, command, super, ctrl, control, alt, option, opt, shift";
const SUPPORTED_KEYS: &str = "single characters, Return, Enter, Tab, Escape, Space, Delete, \
                              Backspace, ForwardDelete, Home, End, PageUp, PageDown, Up, Down, \
                              Left, Right, F1..F12";

/// Parse a chord such as `"cmd+shift+f"` or `"Return"`.
///
/// Tokens split on `+` with no whitespace stripped (a lone `" "` is the
/// Space key); the last token is the key, earlier ones are modifiers.
/// Names are case-insensitive; Enter aliases Return, Backspace aliases Delete.
///
/// # Errors
///
/// `INVALID_ARGUMENT` for an empty or unsupported chord.
pub fn parse_chord(key: &str) -> Result<ParsedChord, ComputerUseError> {
    if key.is_empty() {
        return Err(
            invalid("key chord is empty; expected a chord like cmd+shift+f")
                .with_details(json!({"key": ""})),
        );
    }
    let (modifier_tokens, last) = match key.rsplit_once('+') {
        Some((modifiers, last)) => (Some(modifiers), last),
        None => (None, key),
    };
    let mut modifiers = BTreeSet::new();
    for token in modifier_tokens
        .into_iter()
        .flat_map(|tokens| tokens.split('+'))
    {
        let Some(modifier) = Modifier::parse(token) else {
            return Err(invalid(format!(
                "unknown modifier {}; supported modifiers: {SUPPORTED_MODIFIERS}",
                repr_str(head(token, 24))
            ))
            .with_details(json!({"key": head(key, 32)})));
        };
        modifiers.insert(modifier);
    }
    Ok(ParsedChord {
        modifiers,
        key: canonical_key(last)?,
    })
}

fn canonical_key(token: &str) -> Result<String, ComputerUseError> {
    let lowered = token.to_lowercase();
    if let Some(&(_, named)) = NAMED_KEYS.iter().find(|(alias, _)| *alias == lowered) {
        return Ok(named.to_string());
    }
    if let Some(function) = function_key(&lowered) {
        return Ok(function);
    }
    if token.chars().count() == 1 && keycode(&lowered).is_some() {
        return Ok(lowered);
    }
    Err(invalid(format!(
        "unknown key {}; supported keys: {SUPPORTED_KEYS}",
        repr_str(head(token, 24))
    ))
    .with_details(json!({"key": head(token, 32)})))
}

/// `f1`..`f12` (lowercased) as the canonical `F1`..`F12`.
fn function_key(lowered: &str) -> Option<String> {
    (1..=12)
        .find(|number| lowered == format!("f{number}"))
        .map(|number| format!("F{number}"))
}

#[cfg(test)]
mod tests;
