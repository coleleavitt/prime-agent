//! Ported from the skill's `tests/test_keymap.py`.

use super::*;
use crate::error::ErrorCode;

fn chord(modifiers: &[Modifier], key: &str) -> ParsedChord {
    ParsedChord {
        modifiers: modifiers.iter().copied().collect(),
        key: key.to_string(),
    }
}

#[test]
fn the_keycode_table_matches_the_macos_virtual_keycodes() {
    let expected: &[(&str, u16)] = &[
        ("a", 0),
        ("b", 11),
        ("c", 8),
        ("d", 2),
        ("e", 14),
        ("f", 3),
        ("g", 5),
        ("h", 4),
        ("i", 34),
        ("j", 38),
        ("k", 40),
        ("l", 37),
        ("m", 46),
        ("n", 45),
        ("o", 31),
        ("p", 35),
        ("q", 12),
        ("r", 15),
        ("s", 1),
        ("t", 17),
        ("u", 32),
        ("v", 9),
        ("w", 13),
        ("x", 7),
        ("y", 16),
        ("z", 6),
        ("1", 18),
        ("2", 19),
        ("3", 20),
        ("4", 21),
        ("5", 23),
        ("6", 22),
        ("7", 26),
        ("8", 28),
        ("9", 25),
        ("0", 29),
        ("Return", 36),
        ("Tab", 48),
        ("Space", 49),
        ("Delete", 51),
        ("Escape", 53),
        ("ForwardDelete", 117),
        ("Home", 115),
        ("End", 119),
        ("PageUp", 116),
        ("PageDown", 121),
        ("Up", 126),
        ("Down", 125),
        ("Left", 123),
        ("Right", 124),
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
        ("Backspace", 51),
        (" ", 49),
        ("Enter", 36),
    ];
    for &(name, code) in expected {
        assert_eq!(keycode(name), Some(code), "{name:?}");
    }
    assert_eq!(keycode(""), None);
}

#[test]
fn single_keys_and_aliases_parse_to_canonical_names() {
    assert_eq!(parse_chord("a").unwrap(), chord(&[], "a"));
    assert_eq!(parse_chord("Return").unwrap(), chord(&[], "Return"));
    assert_eq!(parse_chord("Enter").unwrap(), chord(&[], "Return"));
    assert_eq!(parse_chord("Backspace").unwrap(), chord(&[], "Delete"));
    assert_eq!(parse_chord(" ").unwrap(), chord(&[], " "));
    assert_eq!(parse_chord("-").unwrap(), chord(&[], "-"));
    assert_eq!(parse_chord("cmd+-").unwrap(), chord(&[Modifier::Cmd], "-"));
}

#[test]
fn modifiers_canonicalize_case_insensitively() {
    let pairs = [
        ("cmd+c", chord(&[Modifier::Cmd], "c")),
        ("command+c", chord(&[Modifier::Cmd], "c")),
        ("super+c", chord(&[Modifier::Cmd], "c")),
        ("CMD+c", chord(&[Modifier::Cmd], "c")),
        ("ctrl+a", chord(&[Modifier::Ctrl], "a")),
        ("control+a", chord(&[Modifier::Ctrl], "a")),
        ("alt+Tab", chord(&[Modifier::Alt], "Tab")),
        ("option+Tab", chord(&[Modifier::Alt], "Tab")),
        ("opt+Tab", chord(&[Modifier::Alt], "Tab")),
        ("shift+a", chord(&[Modifier::Shift], "a")),
        ("cmd+shift+f", chord(&[Modifier::Cmd, Modifier::Shift], "f")),
    ];
    for (input, expected) in pairs {
        assert_eq!(parse_chord(input).unwrap(), expected, "{input}");
    }
}

#[test]
fn key_names_are_case_insensitive() {
    assert_eq!(parse_chord("RETURN").unwrap(), chord(&[], "Return"));
    assert_eq!(parse_chord("A").unwrap(), chord(&[], "a"));
    assert_eq!(parse_chord("f1").unwrap(), chord(&[], "F1"));
    assert_eq!(parse_chord("F12").unwrap(), chord(&[], "F12"));
}

#[test]
fn invalid_chords_are_invalid_arguments_with_bounded_messages() {
    for bad in [
        "",
        "cmd",
        "cmd+",
        "cmd++c",
        "+c",
        "notmod+c",
        "notakey",
        "ctrl+notakey",
        "F13",
        "f0",
    ] {
        let error = parse_chord(bad).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument, "{bad:?}");
    }
    assert_eq!(
        parse_chord("hyper+x").unwrap_err(),
        invalid(format!(
            "unknown modifier 'hyper'; supported modifiers: {SUPPORTED_MODIFIERS}"
        ))
        .with_details(json!({"key": "hyper+x"}))
    );
    let long = format!("{}+x", "m".repeat(40));
    let error = parse_chord(&long).unwrap_err();
    assert!(
        error
            .message
            .starts_with(&format!("unknown modifier '{}';", "m".repeat(24)))
    );
    assert_eq!(error.details, Some(json!({"key": head(&long, 32)})));
    assert_eq!(
        parse_chord("").unwrap_err(),
        invalid("key chord is empty; expected a chord like cmd+shift+f")
            .with_details(json!({"key": ""}))
    );
}
