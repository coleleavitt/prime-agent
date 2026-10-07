//! A text's lines, indexed once, for here-document bodies.
//!
//! Every guard finds a here-document's end the same way: the first later
//! line that reads as the delimiter. Rescanning the rest of the text for each
//! opener is quadratic on a line of thousands of openers, or on thousands of
//! openers whose delimiter never comes; the index answers each lookup with a
//! binary search instead.

use std::cell::OnceCell;
use std::collections::HashMap;

use super::chars::is_space;

/// How a line is compared with a delimiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKey {
    /// The line exactly.
    Exact,
    /// The line with its leading tabs removed (`<<-`).
    LeadingTabsStripped,
    /// The line with surrounding whitespace removed (Python `str.strip()`).
    Stripped,
}

impl LineKey {
    fn apply(self, line: &[char]) -> String {
        match self {
            LineKey::Exact => line.iter().collect(),
            LineKey::LeadingTabsStripped => line.iter().skip_while(|ch| **ch == '\t').collect(),
            LineKey::Stripped => {
                let start = line
                    .iter()
                    .position(|ch| !is_space(*ch))
                    .unwrap_or(line.len());
                let end = line
                    .iter()
                    .rposition(|ch| !is_space(*ch))
                    .map_or(start, |end| end + 1);
                line[start..end].iter().collect()
            }
        }
    }

    fn slot(self) -> usize {
        match self {
            LineKey::Exact => 0,
            LineKey::LeadingTabsStripped => 1,
            LineKey::Stripped => 2,
        }
    }
}

/// The newline positions of a text and, per [`LineKey`] (built on first
/// use), the starts of the lines with each content.
#[derive(Debug)]
pub(crate) struct LineIndex<'a> {
    chars: &'a [char],
    newlines: Vec<usize>,
    by_content: [OnceCell<HashMap<String, Vec<usize>>>; 3],
}

impl<'a> LineIndex<'a> {
    pub(crate) fn new(chars: &'a [char]) -> Self {
        Self {
            chars,
            newlines: (0..chars.len()).filter(|&i| chars[i] == '\n').collect(),
            by_content: [OnceCell::new(), OnceCell::new(), OnceCell::new()],
        }
    }

    /// The first newline at or after `from`.
    pub(crate) fn newline_from(&self, from: usize) -> Option<usize> {
        let at = self.newlines.partition_point(|&newline| newline < from);
        self.newlines.get(at).copied()
    }

    /// Where the line holding `from` ends: its newline, or the text end.
    pub(crate) fn line_end(&self, from: usize) -> usize {
        self.newline_from(from).unwrap_or(self.chars.len())
    }

    /// The start of the first line that begins after a newline at or after
    /// `from` and whose content, compared by `key`, is `text`.
    pub(crate) fn first_line_from(&self, from: usize, key: LineKey, text: &str) -> Option<usize> {
        let map = self.by_content[key.slot()].get_or_init(|| {
            let mut map: HashMap<String, Vec<usize>> = HashMap::new();
            for &newline in &self.newlines {
                let start = newline + 1;
                let line = &self.chars[start..self.line_end(start)];
                map.entry(key.apply(line)).or_default().push(start);
            }
            map
        });
        let starts = map.get(text)?;
        starts
            .get(starts.partition_point(|&start| start < from))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_follow_the_lines() {
        let chars: Vec<char> = "cat <<A\nx\n\tA\n  A \nA".chars().collect();
        let lines = LineIndex::new(&chars);
        assert_eq!(lines.newline_from(0), Some(7));
        assert_eq!(lines.line_end(8), 9);
        assert_eq!(lines.first_line_from(8, LineKey::Exact, "A"), Some(18));
        assert_eq!(
            lines.first_line_from(8, LineKey::LeadingTabsStripped, "A"),
            Some(10)
        );
        assert_eq!(lines.first_line_from(8, LineKey::Stripped, "A"), Some(10));
        assert_eq!(lines.first_line_from(19, LineKey::Exact, "A"), None);
    }
}
