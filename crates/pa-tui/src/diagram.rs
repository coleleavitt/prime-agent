//! Where the TUI draws Mermaid diagrams, and how often it did.
//!
//! Every `mermaid` fence is laid out by the built-in renderer ([`crate::mermaid::layout`]):
//! drawn rows or the kept source, each with notice lines under it; the TUI ends that
//! paragraph, so following text never joins its last row. Assistant text, custom messages,
//! and `/btw` answers draw through the markdown hook (`markdown::mermaid_blocks`), all
//! under the `markdown.mermaid` mode ([`mode_active`]); plain-text surfaces (agent
//! messages) find their fences with [`text_segments`].
//!
//! Every settled layout also counts once per diagram into the run's [`RenderCounts`]
//! (drawn, kept as source, drawn on the other axis to fit), which the composition root
//! reads with [`take_render_counts`] after turning counting on with
//! [`set_render_counting`].

#[path = "diagram_counts.rs"]
mod counts;

pub(crate) use counts::{Outcome, settled};
pub use counts::{RenderCounts, set_render_counting, take_render_counts};

use crate::mermaid::{ArtSpan, Layout, NoticeLevel};
use crate::width::is_whitespace_char;

/// Whether `mode` draws diagrams in a message that is (or is not) still streaming.
pub(crate) fn mode_active(mode: crate::markdown::MermaidMode, streaming: bool) -> bool {
    use crate::markdown::MermaidMode;
    match mode {
        MermaidMode::Off => false,
        MermaidMode::Final => !streaming,
        MermaidMode::Streaming => true,
    }
}

/// One piece of plain text around drawn diagrams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextSegment {
    /// Prose lines (re-wrapped by the caller); a notice line carries its level.
    Text(Vec<TextLine>),
    /// Drawn diagram rows: they fit the width and must not be re-wrapped.
    Rows(Vec<Vec<ArtSpan>>),
}

/// One source line of a text segment, split into prose and notice fragments.
pub(crate) type TextLine = Vec<(String, Option<NoticeLevel>)>;

/// The `mermaid` fences of plain text the way the markdown blocks find them (a fence line
/// is a line whose trimmed text starts with three backticks; an unclosed fence runs to the
/// end): the text cut into `(raw text, fence source)` pieces that concatenate back to it,
/// the source set only for a `mermaid` fence.
fn plain_blocks(text: &str) -> Vec<(String, Option<String>)> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut prose = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(info) = line.trim().strip_prefix("```") else {
            prose.push_str(line);
            i += 1;
            continue;
        };
        let start = i;
        i += 1;
        while i < lines.len() && !lines[i].trim().starts_with("```") {
            i += 1;
        }
        let end = (i + 1).min(lines.len());
        let mut raw: String = lines[start..end].concat();
        // A closed fence ends at its closing backticks. The newline after them belongs to
        // the blank run that follows, if one does; a lone newline folds into the fence
        // (marked appends a one-newline `space` token to the token before it).
        let closed = i < lines.len();
        let blank_follows = lines.get(end).is_some_and(|next| {
            next.trim_end_matches('\n')
                .trim_matches([' ', '\t'])
                .is_empty()
        });
        let carried = closed && raw.ends_with('\n') && blank_follows;
        if carried {
            raw.pop();
        }
        let is_mermaid = info
            .split(is_whitespace_char)
            .find(|w| !w.is_empty())
            .is_some_and(|w| w.to_lowercase() == "mermaid");
        if is_mermaid {
            if !prose.is_empty() {
                out.push((std::mem::take(&mut prose), None));
            }
            // An indented fence's content loses that indentation where it has as much
            // (marked's `indentCodeCompensation`).
            let fence_indent = line.chars().take_while(|&c| is_whitespace_char(c)).count();
            let body: Vec<&str> = lines[start + 1..i.min(lines.len())]
                .iter()
                .map(|l| {
                    let l = l.strip_suffix('\n').unwrap_or(l);
                    let indent = l.chars().take_while(|&c| is_whitespace_char(c)).count();
                    if fence_indent > 0 && indent >= fence_indent {
                        l.char_indices()
                            .nth(fence_indent)
                            .map_or("", |(at, _)| &l[at..])
                    } else {
                        l
                    }
                })
                .collect();
            out.push((raw, Some(body.join("\n"))));
        } else {
            prose.push_str(&raw);
        }
        if carried {
            prose.push('\n');
        }
        i = end;
    }
    if !prose.is_empty() {
        out.push((prose, None));
    }
    out
}

/// Pending prose: fragments of text whose `\n`s end lines.
type Pending = Vec<(String, Option<NoticeLevel>)>;

/// Remove up to `max` trailing newlines from the pending text (`None`: all of them).
fn trim_trailing_newlines(pending: &mut Pending, max: Option<usize>) {
    let mut removed = 0;
    while max.is_none_or(|max| removed < max) {
        let Some((last, _)) = pending.last_mut() else {
            return;
        };
        if last.is_empty() {
            pending.pop();
            continue;
        }
        if !last.ends_with('\n') {
            return;
        }
        last.pop();
        removed += 1;
    }
}

/// The pending text as lines of fragments.
fn pending_lines(pending: Pending) -> Vec<TextLine> {
    let mut lines: Vec<TextLine> = vec![Vec::new()];
    for (text, level) in pending {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            if !first.is_empty() {
                lines
                    .last_mut()
                    .expect("one line")
                    .push((first.to_owned(), level));
            }
        }
        for part in parts {
            let mut line = Vec::new();
            if !part.is_empty() {
                line.push((part.to_owned(), level));
            }
            lines.push(line);
        }
    }
    lines
}

/// Split plain message text around the diagrams `layout` (source, width, streaming:
/// [`crate::mermaid::layout`], or recorded layouts in tests) draws at `available_width`
/// (never streaming); `None` when nothing was drawn and no notice was added, so the caller
/// keeps its plain rendering.
pub(crate) fn text_segments(
    text: &str,
    available_width: usize,
    layout: &dyn Fn(&str, usize, bool) -> Layout,
) -> Option<Vec<TextSegment>> {
    if !text.contains("mermaid") {
        return None;
    }
    let mut segments = Vec::new();
    let mut pending: Pending = Vec::new();
    let mut changed = false;
    for (raw, source) in plain_blocks(text) {
        let Some(source) = source else {
            pending.push((raw, None));
            continue;
        };
        let shown = layout(&source, available_width, false);
        settled(&source, Outcome::of(&shown));
        let (rows, notices) = match shown {
            Layout::Art { art, notices, .. } => (Some(art.rows), notices),
            Layout::Source { notices } => (None, notices),
        };
        changed |= rows.is_some() || !notices.is_empty();
        let notice_pieces = notices
            .into_iter()
            .map(|notice| (format!("{}\n", notice.text), Some(notice.level)));
        let Some(rows) = rows else {
            // The kept fence ends in exactly one newline, the notices after it.
            pending.push((format!("{}\n", raw.trim_end_matches('\n')), None));
            pending.extend(notice_pieces);
            continue;
        };
        if pending.iter().any(|(text, _)| !text.is_empty()) {
            trim_trailing_newlines(&mut pending, Some(1));
            segments.push(TextSegment::Text(pending_lines(std::mem::take(
                &mut pending,
            ))));
        }
        pending.clear();
        segments.push(TextSegment::Rows(rows));
        pending.extend(notice_pieces);
    }
    if pending.iter().any(|(text, _)| !text.is_empty()) {
        trim_trailing_newlines(&mut pending, None);
        segments.push(TextSegment::Text(pending_lines(pending)));
    }
    changed.then_some(segments)
}

#[cfg(test)]
#[path = "diagram_tests.rs"]
mod tests;
