//! The diagram renderer seam: a process-wide, once-installed renderer that lays out the
//! `mermaid` fences the TUI draws, in place of the built-in renderer (`crate::mermaid`).
//!
//! Without an installed renderer nothing here is consulted and diagrams render exactly as
//! the built-in transform draws them (assistant text only). An installed renderer decides,
//! per fence, between drawn rows and the kept source, each with notice lines under it; the
//! TUI ends that paragraph, so following text never joins its last row. It may also opt
//! into drawing on surfaces beyond assistant text ([`DiagramSurface`]), honouring the same
//! `markdown.mermaid` mode. Plain-text surfaces (agent messages) find their fences with
//! [`text_segments`].
//!
//! Every settled layout also counts once per diagram into the run's [`RenderCounts`]
//! (drawn, kept as source, drawn adapted), which the composition root reads with
//! [`take_render_counts`] after turning counting on with [`set_render_counting`].

use std::sync::OnceLock;

#[path = "diagram_counts.rs"]
mod counts;

pub(crate) use counts::{Outcome, settled};
pub use counts::{RenderCounts, set_render_counting, take_render_counts};

use crate::width::is_whitespace_char;

/// What a run of diagram cells is; the TUI maps roles to its theme slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagramRole {
    /// Box outlines, frames, rules.
    Border,
    /// Node and participant labels.
    Text,
    /// Connector lines and arrowheads.
    Edge,
    /// Text sitting on an edge.
    EdgeLabel,
    /// A diagram title.
    Title,
    /// Blank filler.
    None,
}

/// A run of adjacent diagram cells sharing one role.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagramSpan {
    pub text: String,
    pub role: DiagramRole,
}

/// How loud a notice under a diagram is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeLevel {
    /// Painted muted.
    Info,
    /// Painted in the warning color.
    Warning,
}

/// One line shown under a diagram, or under the source kept in its place.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagramNotice {
    pub level: NoticeLevel,
    pub text: String,
}

/// How one fence is shown at the available width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagramLayout {
    /// Draw these rows in place of the fence (they fit the width), then the notices.
    /// `adapted`: the renderer changed the diagram's layout to make it fit (for example,
    /// drew it on the other axis); it only feeds [`RenderCounts::adapted`].
    Rows {
        rows: Vec<Vec<DiagramSpan>>,
        notices: Vec<DiagramNotice>,
        adapted: bool,
    },
    /// Keep the fence, then the notices (none: the fence is left untouched).
    Source { notices: Vec<DiagramNotice> },
}

/// A surface beyond assistant text where an installed renderer may also draw diagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagramSurface {
    /// The markdown body of a custom message.
    CustomMessage,
    /// The plain-text body of an agent message, received or sent.
    AgentMessage,
    /// A side-question (`/btw`) answer.
    SideAnswer,
}

/// A diagram renderer installed in place of the built-in one.
pub trait DiagramRenderer: Send + Sync {
    /// Lay out one `mermaid` fence's source in `available_width` columns. `streaming` is
    /// set while the message is still arriving.
    fn layout(&self, source: &str, available_width: usize, streaming: bool) -> DiagramLayout;

    /// Whether diagrams also draw on `surface` (assistant text always draws them).
    fn draws_on(&self, surface: DiagramSurface) -> bool {
        let _ = surface;
        false
    }
}

static RENDERER: OnceLock<Box<dyn DiagramRenderer>> = OnceLock::new();

/// A renderer was already installed; the first install stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyInstalled;

impl std::fmt::Display for AlreadyInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a diagram renderer is already installed")
    }
}

impl std::error::Error for AlreadyInstalled {}

/// Install the process's diagram renderer, before the first frame renders.
///
/// # Errors
///
/// [`AlreadyInstalled`] when one is installed already; the first install stays.
pub fn install_diagram_renderer(
    renderer: Box<dyn DiagramRenderer>,
) -> Result<(), AlreadyInstalled> {
    RENDERER.set(renderer).map_err(|_| AlreadyInstalled)
}

/// The installed renderer, if any.
pub(crate) fn installed() -> Option<&'static dyn DiagramRenderer> {
    RENDERER.get().map(AsRef::as_ref)
}

/// Whether `mode` draws diagrams in a message that is (or is not) still streaming.
pub(crate) fn mode_active(mode: crate::markdown::MermaidMode, streaming: bool) -> bool {
    use crate::markdown::MermaidMode;
    match mode {
        MermaidMode::Off => false,
        MermaidMode::Final => !streaming,
        MermaidMode::Streaming => true,
    }
}

/// The markdown diagram setting for `surface`: set only when the installed renderer
/// draws there, so without one the surface keeps its fences.
pub(crate) fn surface_render(
    surface: DiagramSurface,
    mode: crate::markdown::MermaidMode,
    streaming: bool,
) -> Option<crate::markdown::MermaidRender> {
    installed()
        .filter(|renderer| renderer.draws_on(surface))
        .map(|_| crate::markdown::MermaidRender { mode, streaming })
}

/// One piece of plain text around drawn diagrams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextSegment {
    /// Prose lines (re-wrapped by the caller); a notice line carries its level.
    Text(Vec<TextLine>),
    /// Drawn diagram rows: they fit the width and must not be re-wrapped.
    Rows(Vec<Vec<DiagramSpan>>),
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

/// Split plain message text around the diagrams `renderer` draws at `available_width`
/// (never streaming); `None` when nothing was drawn and no notice was added, so the caller
/// keeps its plain rendering.
pub(crate) fn text_segments(
    text: &str,
    available_width: usize,
    renderer: &dyn DiagramRenderer,
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
        let layout = renderer.layout(&source, available_width, false);
        settled(&source, Outcome::of(&layout));
        let (rows, notices) = match layout {
            DiagramLayout::Rows { rows, notices, .. } => (Some(rows), notices),
            DiagramLayout::Source { notices } => (None, notices),
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
