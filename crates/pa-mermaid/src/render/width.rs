//! Display width per grapheme cluster: the cluster is the unit of both measuring and
//! painting, so a box is always sized for exactly what is drawn into it.
//!
//! The measure of one cluster is pluggable ([`crate::set_width_measure`]): a host that pads
//! and wraps the art with its own measure installs it, so a diagram judged to fit really
//! fits the row it is painted in. The default is the package's own rule over the
//! `unicode-width` table: the widest code point wins; a VS16 or a regional-indicator pair
//! forces two columns.

use std::sync::OnceLock;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

static MEASURE: OnceLock<fn(&str) -> usize> = OnceLock::new();

/// Install the cluster measure; the first install wins.
pub(crate) fn set_measure(measure: fn(&str) -> usize) -> bool {
    MEASURE.set(measure).is_ok()
}

/// The package's `clusterWidth`: the widest code point, two for VS16 or a flag pair.
pub(crate) fn package_cluster_width(cluster: &str) -> usize {
    let mut w = 0;
    let mut vs16 = false;
    let mut regional = 0;
    for c in cluster.chars() {
        if c == '\u{fe0f}' {
            vs16 = true;
        }
        if ('\u{1f1e6}'..='\u{1f1ff}').contains(&c) {
            regional += 1;
        }
        // The package's table rates the controls it keeps (`\t\n\r`) one column.
        w = w.max(c.width().unwrap_or(1));
    }
    if vs16 || regional >= 2 {
        2
    } else {
        w
    }
}

fn cluster_width(cluster: &str) -> usize {
    MEASURE.get().map_or_else(
        || package_cluster_width(cluster),
        |measure| measure(cluster),
    )
}

/// Grapheme clusters of `s` paired with their display width.
pub(super) fn measured(s: &str) -> impl Iterator<Item = (&str, usize)> {
    s.graphemes(true).map(|g| (g, cluster_width(g)))
}

/// Display columns of a string.
pub(super) fn string_width(s: &str) -> usize {
    // Printable ASCII is one column a character under every measure.
    if s.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return s.len();
    }
    measured(s).map(|(_, w)| w).sum()
}
