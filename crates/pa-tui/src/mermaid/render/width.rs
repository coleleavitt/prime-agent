//! Display width per grapheme cluster: the cluster is the unit of both measuring and
//! painting, so a box is always sized for exactly what is drawn into it.
//!
//! The package measures a cluster with its generated `unicode-width` table (the widest code
//! point wins; VS16 or a regional-indicator pair forces two). This port measures with
//! pa-tui's [`crate::width::grapheme_width`] instead — the measure the transcript wraps and
//! pads with — so a diagram judged to fit really fits the row it is painted in. The two
//! agree on ASCII, Latin, CJK, Hangul syllables, and emoji (ZWJ sequences, flags, keycaps,
//! skin tones, VS16); they differ on lone controls and marks, a tab (package 1, here 3 —
//! the chat markdown expands tabs before a fence reaches the renderer), and a few
//! East-Asian-narrow marks and Unicode 16/17 emoji.

use unicode_segmentation::UnicodeSegmentation;

use crate::width::grapheme_width;

/// Grapheme clusters of `s` paired with their display width.
pub(super) fn measured(s: &str) -> impl Iterator<Item = (&str, usize)> {
    s.graphemes(true).map(|g| (g, grapheme_width(g)))
}

/// Display columns of a string.
pub(super) fn string_width(s: &str) -> usize {
    // Printable ASCII is one column a character under every measure.
    if s.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return s.len();
    }
    measured(s).map(|(_, w)| w).sum()
}
