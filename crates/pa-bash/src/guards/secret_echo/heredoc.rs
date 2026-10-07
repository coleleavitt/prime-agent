//! Here-document bodies: never shell input, so their lines are skipped by
//! the word checks (and quoted ones by the substitution walk as well).

use std::collections::{BTreeSet, HashMap};

use super::mask::Segment;
use super::words::{shell_words, Reach};

/// An arithmetic command turns `<<` into a shift on its line.
const ARITHMETIC_OPENS: [&str; 2] = ["((", "$["];

/// One here-document opener: its delimiter word and whether it was quoted
/// (a quoted delimiter makes the body literal; an unquoted one still expands
/// command substitutions in it).
struct Declaration {
    delimiter: String,
    quoted: bool,
}

/// Where the first arithmetic opener in `text` ends (as an index into the
/// slice), so "does `text[..position]` contain one" is one comparison per
/// here-document operator instead of a rescan of the line.
fn first_arithmetic_end(text: &[char]) -> Option<usize> {
    ARITHMETIC_OPENS
        .iter()
        .filter_map(|marker| {
            let marker: Vec<char> = marker.chars().collect();
            text.windows(marker.len())
                .position(|window| window == marker.as_slice())
                .map(|at| at + marker.len())
        })
        .min()
}

/// The here-documents opened in `command[start..end]`. The operator is found
/// in the masked text, so a `<<` inside quotes or a comment is data; a line
/// that already opened an arithmetic command (`(( x = 1 << 2 ))`, `$[ ... ]`)
/// keeps its `<<` a shift.
fn declarations(command: &[char], masked: &[char], start: usize, end: usize) -> Vec<Declaration> {
    let mut found = Vec::new();
    let arithmetic_end = first_arithmetic_end(&command[start..end]).map(|end| start + end);
    let mut index = start;
    while index < end {
        if masked[index] != '<' {
            index += 1;
            continue;
        }
        let mut run_end = index;
        while run_end < end && masked[run_end] == '<' {
            run_end += 1;
        }
        if run_end - index == 2 {
            let mut position = run_end;
            if position < end && masked[position] == '-' {
                position += 1;
            }
            while position < end && matches!(command[position], ' ' | '\t') {
                position += 1;
            }
            if position < end {
                let words = shell_words(command, position, end, Reach::FirstWord);
                let arithmetic = arithmetic_end.is_some_and(|end| end <= position);
                if let (Some(word), false) = (words.into_iter().next(), arithmetic) {
                    found.push(Declaration {
                        delimiter: word.text,
                        quoted: matches!(command[position], '\'' | '"'),
                    });
                }
            }
        }
        index = run_end;
    }
    found
}

/// Segment indices of every here-document body, of the quoted bodies, and
/// of the lines that close a body.
#[derive(Debug, Default)]
pub(super) struct HeredocSegments {
    pub bodies: BTreeSet<usize>,
    pub quoted_bodies: BTreeSet<usize>,
    pub delimiter_lines: BTreeSet<usize>,
}

/// Find the here-document bodies of a command, in one forward pass.
///
/// A body runs to the first later line whose only word is its delimiter, and
/// a line opening several (`cat <<'A' <<'B'`) claims its bodies in order. A
/// body whose delimiter never appears claims nothing. The closing line is
/// reported separately: the walk skips it (it expands nothing), while the word
/// checks still read it (`cat <<'env'` closed by `env` stays refused).
pub(super) fn heredoc_segments(
    command: &[char],
    masked: &[char],
    segments: &[Segment],
) -> HeredocSegments {
    let mut lines: Vec<(usize, usize)> = Vec::new();
    let mut delimiter_lines: HashMap<String, Vec<usize>> = HashMap::new();
    let mut index = 0;
    while index < segments.len() {
        let mut last = index;
        while last + 1 < segments.len() && segments[last].separator != Some('\n') {
            last += 1;
        }
        lines.push((index, last));
        let mut words = shell_words(
            command,
            segments[index].start,
            segments[last].end,
            Reach::All,
        );
        if words.len() == 1 {
            let word = words.remove(0);
            delimiter_lines
                .entry(word.text)
                .or_default()
                .push(lines.len() - 1);
        }
        index = last + 1;
    }
    let mut found = HeredocSegments::default();
    let mut cursors: HashMap<&str, usize> = HashMap::new();
    let mut line_index = 0;
    while line_index < lines.len() {
        let (first, last) = lines[line_index];
        let mut body_line = line_index + 1;
        for declaration in declarations(command, masked, segments[first].start, segments[last].end)
        {
            let Some((delimiter, candidates)) =
                delimiter_lines.get_key_value(&declaration.delimiter)
            else {
                break;
            };
            let cursor = cursors.entry(delimiter.as_str()).or_insert(0);
            while *cursor < candidates.len() && candidates[*cursor] < body_line {
                *cursor += 1;
            }
            let Some(&closing) = candidates.get(*cursor) else {
                break;
            };
            for &(first, last) in &lines[body_line..closing] {
                let span = first..=last;
                found.bodies.extend(span.clone());
                if declaration.quoted {
                    found.quoted_bodies.extend(span);
                }
            }
            found
                .delimiter_lines
                .extend(lines[closing].0..=lines[closing].1);
            body_line = closing + 1;
        }
        line_index = body_line;
    }
    found
}
