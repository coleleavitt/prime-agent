//! Display width per grapheme cluster: the cluster is the unit of both measuring and
//! painting, so a box is always sized for exactly what is drawn into it.
//!
//! The package measures a cluster with its generated `unicode-width` table (the widest code
//! point wins; VS16 or a regional-indicator pair forces two). This port measures with
//! pa-tui's [`crate::width::grapheme_width`] instead — the measure the transcript wraps and
//! pads with — so a diagram the hook judges to fit really fits the row it is painted in.
//!
//! A census of every code point (each as a lone cluster) against the package's table found
//! them equal everywhere except: C0/C1 controls, spacing combining marks (Mc), and the
//! prepended concatenation marks standing alone (package 1, here 0 — controls other than
//! `\t\n\r` are stripped before parsing, and a mark after a base measures as the base on
//! both sides); `\t` (package 1, here 3 — the chat markdown expands tabs before the hook
//! runs); the Hangul medial/final jamo, U+FF9E/U+FF9F, and nine other EAW-narrow marks
//! (package 0, here 1, the TS `visibleWidth` rule); U+115F, U+16FF0/1, and a lone U+FE0F
//! (package 2, here 0); lone regional indicators and thirteen Unicode 16/17 emoji
//! (package 1, here 2). ASCII, Latin, CJK, Hangul syllables, and emoji — ZWJ sequences,
//! flags, keycaps, skin tones, VS16 — agree; the golden corpus covers CJK and emoji labels.

use crate::width::grapheme_width;
use unicode_segmentation::UnicodeSegmentation;

/// Grapheme clusters of `s` paired with their display width.
pub(super) fn measured(s: &str) -> impl Iterator<Item = (&str, usize)> {
    s.graphemes(true).map(|g| (g, grapheme_width(g)))
}

/// Display columns of a string.
pub(super) fn string_width(s: &str) -> usize {
    if s.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return s.len();
    }
    measured(s).map(|(_, w)| w).sum()
}
