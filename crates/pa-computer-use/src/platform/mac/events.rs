//! The `CGEvent` sequences one input action posts to the app's process.
//!
//! Every event goes to the target pid (`CGEventPostToPid`), never to the
//! global event stream; coordinates are CG screen-space points the session
//! already mapped from window-relative ones.

use std::collections::BTreeSet;

use crate::element::Pair;
use crate::error::{Result, invalid};
use crate::keymap::{Modifier, ParsedChord, keycode};
use crate::platform::{MouseButton, ScrollDirection};

/// One page of scrolling, in pixels.
const PIXELS_PER_PAGE: i32 = 800;
/// A keyboard event carries at most two UTF-16 code units (the macOS limit).
const UTF16_UNITS_PER_EVENT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseKind {
    Down,
    Up,
    /// A left-button drag motion.
    Dragged,
}

/// One event to post.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MacEvent {
    Mouse {
        kind: MouseKind,
        button: MouseButton,
        point: Pair,
    },
    /// A two-wheel pixel scroll: `vertical` is wheel 1, `horizontal` wheel 2.
    Scroll {
        vertical: i32,
        horizontal: i32,
        location: Pair,
    },
    Key {
        keycode: u16,
        down: bool,
        modifiers: BTreeSet<Modifier>,
    },
    /// A key event (virtual keycode 0) carrying literal UTF-16 text.
    Unicode { down: bool, units: Vec<u16> },
}

/// `count` press/release cycles (a double click is two).
pub(crate) fn click(point: Pair, button: MouseButton, count: u32) -> Vec<MacEvent> {
    (0..count)
        .flat_map(|_| {
            [MouseKind::Down, MouseKind::Up].map(|kind| MacEvent::Mouse {
                kind,
                button,
                point,
            })
        })
        .collect()
}

/// Press at `from`, drag to `to`, release there (left button).
pub(crate) fn drag(from: Pair, to: Pair) -> Vec<MacEvent> {
    let button = MouseButton::Left;
    vec![
        MacEvent::Mouse {
            kind: MouseKind::Down,
            button,
            point: from,
        },
        MacEvent::Mouse {
            kind: MouseKind::Dragged,
            button,
            point: to,
        },
        MacEvent::Mouse {
            kind: MouseKind::Up,
            button,
            point: to,
        },
    ]
}

/// One scroll of `pages` pages at `location`: up/down a negative/positive
/// vertical delta, left/right a negative/positive horizontal one.
pub(crate) fn scroll(direction: ScrollDirection, pages: u32, location: Pair) -> MacEvent {
    let magnitude = i32::try_from(pages)
        .unwrap_or(i32::MAX)
        .saturating_mul(PIXELS_PER_PAGE);
    let (vertical, horizontal) = match direction {
        ScrollDirection::Up => (-magnitude, 0),
        ScrollDirection::Down => (magnitude, 0),
        ScrollDirection::Left => (0, -magnitude),
        ScrollDirection::Right => (0, magnitude),
    };
    MacEvent::Scroll {
        vertical,
        horizontal,
        location,
    }
}

/// The chord's key down and up, both carrying its modifier flags.
///
/// # Errors
///
/// `INVALID_ARGUMENT` for a key without a macOS keycode (the parser only
/// yields keys that have one).
pub(crate) fn chord(chord: &ParsedChord) -> Result<Vec<MacEvent>> {
    let Some(code) = keycode(&chord.key) else {
        return Err(invalid(format!("unsupported key: {}", chord.key)));
    };
    Ok([true, false]
        .map(|down| MacEvent::Key {
            keycode: code,
            down,
            modifiers: chord.modifiers.clone(),
        })
        .to_vec())
}

/// `text` as key down/up pairs of at most two UTF-16 units each, never
/// splitting a surrogate pair.
pub(crate) fn text(text: &str) -> Vec<MacEvent> {
    let mut chunks: Vec<Vec<u16>> = Vec::new();
    let mut pending: Vec<u16> = Vec::new();
    for character in text.chars() {
        let mut buffer = [0_u16; 2];
        let units = character.encode_utf16(&mut buffer);
        if pending.len() + units.len() > UTF16_UNITS_PER_EVENT {
            chunks.push(std::mem::take(&mut pending));
        }
        pending.extend_from_slice(units);
    }
    if !pending.is_empty() {
        chunks.push(pending);
    }
    chunks
        .into_iter()
        .flat_map(|units| {
            [true, false].map(|down| MacEvent::Unicode {
                down,
                units: units.clone(),
            })
        })
        .collect()
}
