//! Mermaid fences become Unicode diagrams (the fork's TS `components/mermaid.ts`,
//! `createMermaidMarkdownTransform`, `a358fd19e`).
//!
//! TS rewrites the source before the markdown lexer runs: each top-level fence whose info
//! string's first word is `mermaid` is laid out ([`crate::mermaid::layout`]) and becomes its
//! diagram rows as inline code spans joined by hard breaks, then its notices — so the rows
//! are a paragraph that joins a paragraph directly above (no blank line between), and the
//! paragraph ends after them. A fence kept as source gains its notices as a paragraph right
//! under it. This module applies the same rewrite to the parsed blocks, keeping the
//! per-cell diagram roles the code-span text could not carry here (ratatui spans, not ANSI
//! strings).
//!
//! Known gap: a fence glued to a list item or a blockquote line (no blank line between)
//! lazily continues that item in marked's re-lex; this port starts a fresh paragraph there,
//! as its block parser does for any non-indented line after a list.

use ratatui::style::Style;

use super::{Block, BlockKind, MarkdownStyle, render_inline};
use crate::diagram::Outcome;
use crate::mermaid::{ArtSpan, Layout, Notice, NoticeLevel, Role};
use crate::{Line, Span};

/// The `markdown.mermaid` setting: when assistant text renders Mermaid fences as diagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MermaidMode {
    /// Never: fences stay code blocks.
    Off,
    /// Once the message settles; a streaming message shows the fence.
    Final,
    /// While streaming too (the TS default).
    #[default]
    Streaming,
}

impl MermaidMode {
    /// The persisted setting value (`off` / `final`; anything else is the default).
    #[must_use]
    pub fn from_setting(value: &str) -> Self {
        match value {
            "off" => Self::Off,
            "final" => Self::Final,
            _ => Self::Streaming,
        }
    }
}

/// The transform's inputs for one text block: the setting and the message's streaming state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MermaidRender {
    pub(crate) mode: MermaidMode,
    pub(crate) streaming: bool,
}

/// The theme slots the diagram classes paint with (TS `styleSpan`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct MermaidPalette {
    pub(crate) border: Style,
    pub(crate) text: Style,
    pub(crate) edge: Style,
    pub(crate) edge_label: Style,
    pub(crate) warning: Style,
    pub(crate) title: Style,
    pub(crate) info: Style,
}

impl MermaidPalette {
    pub(crate) fn from_theme(theme: &crate::theme::Theme) -> Self {
        use crate::theme::ThemeColor as C;
        Self {
            border: theme.fg_style(C::BorderMuted),
            text: theme.fg_style(C::Text),
            edge: theme.fg_style(C::Accent),
            edge_label: theme.fg_style(C::Muted),
            warning: theme.fg_style(C::Warning),
            title: theme
                .fg_style(C::Accent)
                .add_modifier(ratatui::style::Modifier::BOLD),
            info: theme.fg_style(C::Muted),
        }
    }
}

/// One source row of a paragraph that carries diagram output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ArtRow {
    /// A soft-break line of ordinary paragraph text (inline markdown).
    Inline(String),
    /// One diagram row (an inline code span in TS).
    Drawn(Vec<ArtSpan>),
    /// A notice under the diagram (or under the kept source).
    Notice(Notice),
}

/// The fence's diagram source, when the block is a `mermaid` fence.
fn mermaid_source(block: &Block) -> Option<String> {
    let BlockKind::Code { lang: Some(lang) } = &block.kind else {
        return None;
    };
    // TS: `lang.trim().split(/\s+/, 1)[0].toLowerCase() === "mermaid"`.
    let first = lang
        .split(crate::width::is_whitespace_char)
        .find(|w| !w.is_empty())?;
    (first.to_lowercase() == "mermaid").then(|| block.lines.join("\n"))
}

/// A paragraph row that is no longer the paragraph's last loses its trailing whitespace
/// (only a block's final source line keeps it).
fn settle_last_row(rows: &mut [ArtRow]) {
    match rows.last_mut() {
        Some(ArtRow::Inline(text)) => text.truncate(text.trim_end().len()),
        Some(ArtRow::Drawn(_) | ArtRow::Notice(_)) | None => {}
    }
}

/// Append `rows` to the paragraph-like block the output ends with when `joins` (no blank
/// line between), else start a new diagram paragraph.
fn push_rows(out: &mut Vec<Block>, sep_blank: bool, rows: Vec<ArtRow>) {
    if !sep_blank {
        if let Some(last) = out.last_mut() {
            if let BlockKind::Paragraph = last.kind {
                let mut inline: Vec<ArtRow> = std::mem::take(&mut last.lines)
                    .into_iter()
                    .map(ArtRow::Inline)
                    .collect();
                settle_last_row(&mut inline);
                inline.extend(rows);
                last.kind = BlockKind::ArtParagraph { rows: inline };
                return;
            }
            if let BlockKind::ArtParagraph { rows: existing } = &mut last.kind {
                settle_last_row(existing);
                existing.extend(rows);
                return;
            }
        }
    }
    out.push(Block {
        kind: BlockKind::ArtParagraph { rows },
        sep_blank,
        lines: Vec::new(),
    });
}

/// Rewrite the parsed blocks, honouring the mode: drawn rows replace each `mermaid` fence
/// (joining a paragraph directly above), the notices follow as rows of the same
/// paragraph; a kept fence gains its notices as a paragraph right under it. Either way the
/// paragraph ends there: the next block starts after a blank line, so following text never
/// joins the last row. Anything else passes through untouched.
pub(super) fn apply(blocks: Vec<Block>, width: usize, style: &MarkdownStyle) -> Vec<Block> {
    apply_with(blocks, width, style, &crate::mermaid::layout)
}

/// [`apply`] with the fences laid out by `layout` (source, width, streaming; tests replay
/// recorded layouts).
pub(super) fn apply_with(
    blocks: Vec<Block>,
    width: usize,
    style: &MarkdownStyle,
    layout: &dyn Fn(&str, usize, bool) -> Layout,
) -> Vec<Block> {
    let Some(render) = style.mermaid else {
        return blocks;
    };
    if !crate::diagram::mode_active(render.mode, render.streaming)
        || !blocks.iter().any(|b| mermaid_source(b).is_some())
    {
        return blocks;
    }
    let streaming = render.streaming;
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut closed = false;
    for mut block in blocks {
        block.sep_blank |= std::mem::take(&mut closed);
        let Some(source) = mermaid_source(&block) else {
            out.push(block);
            continue;
        };
        let shown = layout(&source, width, streaming);
        if !streaming {
            crate::diagram::settled(&source, Outcome::of(&shown));
        }
        match shown {
            Layout::Art { art, notices, .. } => {
                let rows = art
                    .rows
                    .into_iter()
                    .map(ArtRow::Drawn)
                    .chain(notices.into_iter().map(ArtRow::Notice))
                    .collect();
                push_rows(&mut out, block.sep_blank, rows);
                closed = true;
            }
            Layout::Source { notices } if notices.is_empty() => out.push(block),
            Layout::Source { notices } => {
                out.push(block);
                out.push(Block {
                    kind: BlockKind::ArtParagraph {
                        rows: notices.into_iter().map(ArtRow::Notice).collect(),
                    },
                    sep_blank: false,
                    lines: Vec::new(),
                });
                closed = true;
            }
        }
    }
    out
}

/// One diagram row: its roles take the theme slots, its blank runs `blank(i)` (the run's
/// index in the row).
pub(crate) fn drawn_spans(
    spans: &[ArtSpan],
    palette: &MermaidPalette,
    blank: impl Fn(usize) -> Style,
) -> Line {
    spans
        .iter()
        .enumerate()
        .map(|(i, span)| {
            let slot = match span.role {
                Role::Border => palette.border,
                Role::Text => palette.text,
                Role::Edge => palette.edge,
                Role::EdgeLabel => palette.edge_label,
                Role::Title => palette.title,
                Role::None => blank(i),
            };
            Span::styled(span.text.clone(), slot)
        })
        .collect()
}

/// One diagram row painted in the theme (TS `styleSpan` inside the row's `mdCode` span):
/// the leading blank run sits in the code color, a later one after a role's color reset.
fn drawn_line(spans: &[ArtSpan], style: &MarkdownStyle) -> Line {
    if spans.is_empty() {
        // A blank diagram row is a no-break space so the row keeps its height.
        return vec![Span::styled("\u{a0}", style.code)];
    }
    drawn_spans(spans, &style.mermaid_palette, |i| {
        if i == 0 { style.code } else { Style::default() }
    })
}

/// The style a notice paints with.
pub(crate) fn notice_style(level: NoticeLevel, palette: &MermaidPalette) -> Style {
    match level {
        NoticeLevel::Info => palette.info,
        NoticeLevel::Warning => palette.warning,
    }
}

/// The paragraph's unwrapped rows; paint wraps them and the row count measures the same.
pub(super) fn art_paragraph_lines(rows: &[ArtRow], style: &MarkdownStyle) -> Vec<Line> {
    rows.iter()
        .map(|row| match row {
            ArtRow::Inline(text) => render_inline(text, style),
            ArtRow::Drawn(spans) => drawn_line(spans, style),
            ArtRow::Notice(notice) => vec![Span::styled(
                notice.text.clone(),
                notice_style(notice.level, &style.mermaid_palette),
            )],
        })
        .collect()
}
