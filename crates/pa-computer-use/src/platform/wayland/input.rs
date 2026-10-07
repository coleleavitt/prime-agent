//! The compositor's virtual pointer and keyboard, as the backend drives them.
//!
//! Input is focus-bound, never window-targeted: the backend focuses and
//! verifies the bound window first. The keyboard uploads our own xkb keymap
//! binding one keycode per keysym, so typing is layout-independent (every
//! character is typed through its own Unicode keysym, the approach `wtype`
//! uses).

use serde_json::json;

use crate::element::Pair;
use crate::error::{invalid, Result};
use crate::keymap::{Modifier, ParsedChord};
use crate::platform::{MouseButton, ScrollDirection};

/// Where the pointer maps: one output by name, in its logical size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PointerTarget {
    pub output: Option<String>,
    pub width: i32,
    pub height: i32,
}

/// One key to press and release with the given modifier mask held.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct KeyStroke {
    pub keysym: String,
    pub modifiers: u32,
}

impl KeyStroke {
    pub(crate) fn plain(keysym: impl Into<String>) -> Self {
        Self {
            keysym: keysym.into(),
            modifiers: 0,
        }
    }
}

/// Which virtual-input managers the compositor offers this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Availability {
    pub pointer: bool,
    pub keyboard: bool,
}

/// The virtual-input protocols (faked in tests).
pub(crate) trait VirtualInput: Send + Sync {
    /// Which managers are advertised (no input is sent).
    fn available(&self) -> Result<Availability>;
    /// Move to `point` (target-logical) and click `count` times.
    fn click(
        &self,
        target: &PointerTarget,
        point: Pair,
        button: MouseButton,
        count: u32,
    ) -> Result<()>;
    /// Press at `start`, move through intermediate points to `end`, release.
    fn drag(&self, target: &PointerTarget, start: Pair, end: Pair) -> Result<()>;
    /// Move to `point`, then send `clicks` discrete wheel clicks.
    fn scroll(
        &self,
        target: &PointerTarget,
        point: Pair,
        direction: ScrollDirection,
        clicks: u32,
    ) -> Result<()>;
    /// Deliver the strokes to whatever surface holds keyboard focus.
    fn send_keys(&self, strokes: &[KeyStroke]) -> Result<()>;
}

/// The real modifier masks of the generated keymap (xkb types/compat "complete").
pub(crate) fn modifier_mask(modifier: Modifier) -> u32 {
    match modifier {
        Modifier::Shift => 1,
        Modifier::Ctrl => 4,
        Modifier::Alt => 8,
        Modifier::Cmd => 64,
    }
}

/// The keysym that types one character (a newline is Return, a tab Tab).
///
/// # Errors
///
/// `INVALID_ARGUMENT` for other control characters.
pub(crate) fn keysym_for_char(character: char) -> Result<String> {
    match character {
        '\n' => return Ok("Return".to_string()),
        '\t' => return Ok("Tab".to_string()),
        _ if character.is_ascii_alphanumeric() => return Ok(character.to_string()),
        _ => {}
    }
    let code = u32::from(character);
    if code < 0x20 || (0x7F..0xA0).contains(&code) {
        return Err(
            invalid(format!("cannot type control character U+{code:04X}"))
                .with_details(json!({"codepoint": code})),
        );
    }
    Ok(format!("U{code:04X}"))
}

/// One chord as a keysym plus the modifier mask of our keymap (cmd is the
/// Logo/super modifier).
pub(crate) fn chord_stroke(chord: &ParsedChord) -> Result<KeyStroke> {
    let keysym = if let Some(named) = crate::platform::x11::named_keysym(&chord.key) {
        named.to_string()
    } else {
        let mut characters = chord.key.chars();
        match (characters.next(), characters.next()) {
            (Some(only), None) => keysym_for_char(only)?,
            _ => chord.key.clone(),
        }
    };
    Ok(KeyStroke {
        keysym,
        modifiers: chord
            .modifiers
            .iter()
            .map(|&modifier| modifier_mask(modifier))
            .fold(0, |mask, bit| mask | bit),
    })
}

/// The xkb keycode of our first key; the evdev code sent is keycode - 8.
pub(crate) const FIRST_KEYCODE: usize = 9;
const MAX_KEYS_PER_KEYMAP: usize = 240;

/// An xkb keymap binding keycodes 9.. to one keysym each.
pub(crate) fn keymap_text(keysyms: &[String]) -> String {
    use std::fmt::Write;
    let last = FIRST_KEYCODE + keysyms.len().max(1) - 1;
    let mut codes = String::new();
    let mut symbols = String::new();
    for (index, keysym) in keysyms.iter().enumerate() {
        let _ = writeln!(codes, "<K{index}> = {};", FIRST_KEYCODE + index);
        let _ = writeln!(symbols, "key <K{index}> {{[ {keysym} ]}};");
    }
    format!(
        "xkb_keymap {{\nxkb_keycodes \"(unnamed)\" {{\nminimum = 8;\nmaximum = {};\n{codes}}};\n\
         xkb_types \"(unnamed)\" {{ include \"complete\" }};\n\
         xkb_compat \"(unnamed)\" {{ include \"complete\" }};\n\
         xkb_symbols \"(unnamed)\" {{\n{symbols}}};\n}};\n",
        last.max(9)
    )
}

/// Split strokes into runs whose distinct keysyms fit one keymap.
pub(crate) fn groups(strokes: &[KeyStroke]) -> Vec<&[KeyStroke]> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut seen: Vec<&str> = Vec::new();
    for (index, stroke) in strokes.iter().enumerate() {
        if !seen.contains(&stroke.keysym.as_str()) && seen.len() >= MAX_KEYS_PER_KEYMAP {
            groups.push(&strokes[start..index]);
            start = index;
            seen.clear();
        }
        if !seen.contains(&stroke.keysym.as_str()) {
            seen.push(&stroke.keysym);
        }
    }
    if start < strokes.len() {
        groups.push(&strokes[start..]);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorCode;
    use crate::keymap::parse_chord;

    #[test]
    fn keysyms_name_characters_and_refuse_control_characters() {
        let keysyms: Vec<String> = "aZ9 !\n\té😀"
            .chars()
            .map(|c| keysym_for_char(c).unwrap())
            .collect();
        assert_eq!(
            keysyms,
            ["a", "Z", "9", "U0020", "U0021", "Return", "Tab", "U00E9", "U1F600"]
        );
        for character in ['\u{1b}', '\r', '\u{7f}'] {
            assert_eq!(
                keysym_for_char(character).unwrap_err().code,
                ErrorCode::InvalidArgument
            );
        }
    }

    #[test]
    fn chords_translate_to_keysyms_and_masks() {
        let stroke = |chord: &str| chord_stroke(&parse_chord(chord).unwrap()).unwrap();
        assert_eq!(
            stroke("cmd+shift+s"),
            KeyStroke {
                keysym: "s".to_string(),
                modifiers: 65
            }
        );
        assert_eq!(
            stroke("ctrl+alt+Delete"),
            KeyStroke {
                keysym: "BackSpace".to_string(),
                modifiers: 12
            }
        );
        assert_eq!(stroke("PageDown"), KeyStroke::plain("Next"));
        assert_eq!(
            stroke("ctrl+."),
            KeyStroke {
                keysym: "U002E".to_string(),
                modifiers: 4
            }
        );
        assert_eq!(stroke(" "), KeyStroke::plain("U0020"));
        assert_eq!(stroke("F12"), KeyStroke::plain("F12"));
    }

    #[test]
    fn groups_split_at_the_keymap_size() {
        let strokes: Vec<KeyStroke> = (0..300)
            .map(|index| KeyStroke::plain(format!("U{:04X}", 0x4E00 + index)))
            .collect();
        let groups = groups(&strokes);
        assert_eq!(
            groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
            [240, 60]
        );
        assert_eq!(groups.concat(), strokes);
        let repeated: Vec<KeyStroke> = (0..500).map(|_| KeyStroke::plain("a")).collect();
        assert_eq!(super::groups(&repeated).len(), 1);
    }

    #[test]
    fn the_keymap_binds_one_keycode_per_keysym() {
        let text = keymap_text(&["s".to_string(), "U00E9".to_string()]);
        assert!(text.contains("<K0> = 9;\n<K1> = 10;\n"));
        assert!(text.contains("key <K0> {[ s ]};\nkey <K1> {[ U00E9 ]};\n"));
        assert!(text.contains("maximum = 10;"));
        assert!(!text.contains("<K2>"));
        assert!(keymap_text(&[]).contains("maximum = 9;"));
    }

    /// Compiles one keymap with the system's libxkbcommon through Python's
    /// ctypes (the skill's test did the same; no FFI in this crate's tests)
    /// and prints the Shift, Control, Mod1 and Mod4 indices, or `SKIP`.
    const XKB_PROBE: &str = r#"
import ctypes, ctypes.util, json, sys
name = ctypes.util.find_library("xkbcommon")
if not name:
    print("SKIP"); sys.exit(0)
lib = ctypes.CDLL(name)
lib.xkb_context_new.restype = ctypes.c_void_p
lib.xkb_keymap_new_from_string.restype = ctypes.c_void_p
lib.xkb_keymap_new_from_string.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
lib.xkb_keymap_mod_get_index.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
lib.xkb_keymap_mod_get_index.restype = ctypes.c_uint
lib.xkb_keymap_unref.argtypes = [ctypes.c_void_p]
lib.xkb_context_unref.argtypes = [ctypes.c_void_p]
context = lib.xkb_context_new(0)
keymap = lib.xkb_keymap_new_from_string(context, open(sys.argv[1], "rb").read(), 1, 0)
if not keymap:
    print("FAILED"); sys.exit(0)
print(json.dumps([lib.xkb_keymap_mod_get_index(keymap, m.encode()) for m in ("Shift", "Control", "Mod1", "Mod4")]))
lib.xkb_keymap_unref(keymap)
lib.xkb_context_unref(context)
"#;

    #[test]
    fn the_generated_keymap_compiles_with_libxkbcommon() {
        let mut keysyms: Vec<String> = "aZ1 !\n\t\u{e9}\u{20ac}"
            .chars()
            .map(|character| keysym_for_char(character).unwrap())
            .collect();
        keysyms.extend(["BackSpace", "Prior", "F12", "Escape"].map(String::from));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keymap.xkb");
        std::fs::write(&path, keymap_text(&keysyms)).unwrap();
        let Ok(output) = std::process::Command::new("python3")
            .args(["-I", "-c", XKB_PROBE])
            .arg(&path)
            .output()
        else {
            eprintln!("python3 not found; skipping the libxkbcommon compile check");
            return;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stdout = stdout.trim();
        if stdout == "SKIP" {
            eprintln!("libxkbcommon not installed; skipping the compile check");
            return;
        }
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_ne!(
            stdout, "FAILED",
            "libxkbcommon refused the generated keymap"
        );
        let indices: Vec<u32> = serde_json::from_str(stdout).unwrap();
        let masks: Vec<u32> = indices.into_iter().map(|index| 1 << index).collect();
        assert_eq!(
            masks,
            [
                Modifier::Shift,
                Modifier::Ctrl,
                Modifier::Alt,
                Modifier::Cmd
            ]
            .map(modifier_mask)
        );
    }
}
