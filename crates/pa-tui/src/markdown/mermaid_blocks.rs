//! Mermaid fences in assistant text become Unicode diagrams (TS `components/mermaid.ts`,
//! the `createMermaidMarkdownTransform` markdown rewrite).
//!
//! TS rewrites the source before the markdown lexer runs: each top-level fence whose info
//! string's first word is `mermaid` becomes its diagram rows as inline code spans joined by
//! hard breaks — so the rows are a paragraph, and they flow into a paragraph directly above
//! or below them (no blank line between) exactly as the re-lexed text would. This module
//! applies the same rewrite to the parsed blocks, keeping the per-cell diagram classes the
//! code-span text could not carry here (ratatui spans, not ANSI strings).
//!
//! Known gap: a fence glued to a list item or a blockquote line (no blank line between)
//! lazily continues that item in marked's re-lex; this port starts a fresh paragraph there,
//! as its block parser does for any non-indented line after a list.
//!
//! With a renderer installed through [`crate::diagram`], each fence is laid out by it
//! instead ([`apply_layouts`]); without one this module draws exactly as above.

use ratatui::style::Style;

use super::{Block, BlockKind, MarkdownStyle, render_inline};
use crate::diagram::{
    DiagramLayout,
    DiagramNotice,
    DiagramRenderer,
    DiagramRole,
    DiagramSpan,
    NoticeLevel,
    Outcome,
};
use crate::mermaid::{ArtSpan, Cls, render_cached};
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
    /// An installed renderer's titles and info notices.
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
    Art(Vec<ArtSpan>),
    /// The `Mermaid diagram not rendered: …` notice under a fence that kept its source;
    /// `trailing` is the hard-break spacing a paragraph's final row keeps.
    Warning { text: String, trailing: String },
    /// One row an installed renderer drew.
    Drawn(Vec<DiagramSpan>),
    /// A notice an installed renderer put under its diagram (or the kept source).
    Notice(DiagramNotice),
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

/// The notice for a final diagram that dropped source (TS: first warning, `(+N more)`).
fn warning_text(warnings: &[String]) -> String {
    let more = match warnings.len() {
        0 | 1 => String::new(),
        n => format!(" (+{} more)", n - 1),
    };
    format!("Mermaid diagram not rendered: {}{more}", warnings[0])
}

/// A paragraph row that is no longer the paragraph's last loses its trailing whitespace
/// (only a block's final source line keeps it).
fn settle_last_row(rows: &mut [ArtRow]) {
    match rows.last_mut() {
        Some(ArtRow::Inline(text)) => text.truncate(text.trim_end().len()),
        Some(ArtRow::Warning { trailing, .. }) => trailing.clear(),
        Some(ArtRow::Art(_) | ArtRow::Drawn(_) | ArtRow::Notice(_)) | None => {}
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

/// Rewrite the parsed blocks: each `mermaid` fence whose diagram fits `width` becomes its
/// rows, honouring the mode; a settled diagram with warnings keeps its fence and gains the
/// notice row. Anything else passes through untouched. An installed renderer
/// ([`crate::diagram`]) lays the fences out instead.
pub(super) fn apply(blocks: Vec<Block>, width: usize, style: &MarkdownStyle) -> Vec<Block> {
    apply_with(blocks, width, style, crate::diagram::installed())
}

/// [`apply`] with an explicit renderer (`None`: the built-in one).
pub(super) fn apply_with(
    blocks: Vec<Block>,
    width: usize,
    style: &MarkdownStyle,
    renderer: Option<&dyn DiagramRenderer>,
) -> Vec<Block> {
    let Some(render) = style.mermaid else {
        return blocks;
    };
    if !crate::diagram::mode_active(render.mode, render.streaming)
        || !blocks.iter().any(|b| mermaid_source(b).is_some())
    {
        return blocks;
    }
    if let Some(renderer) = renderer {
        return apply_layouts(blocks, width, render.streaming, renderer);
    }

    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    // The output ends with diagram rows whose trailing newline a directly following
    // paragraph continues (TS: the rows end in `\n` and the next token's text follows).
    let mut rows_open = false;
    let mut blocks = blocks.into_iter().peekable();
    while let Some(block) = blocks.next() {
        if rows_open && !block.sep_blank {
            if let BlockKind::Paragraph = block.kind {
                push_rows(
                    &mut out,
                    /*sep_blank*/ false,
                    block.lines.into_iter().map(ArtRow::Inline).collect(),
                );
                continue;
            }
        }
        rows_open = false;
        let Some(source) = mermaid_source(&block) else {
            out.push(block);
            continue;
        };
        let art = render_cached(&source);
        let shown = art.as_ref().as_ref().filter(|art| art.width <= width);
        if !render.streaming {
            let drawn = shown.is_some_and(|art| art.warnings.is_empty());
            let outcome = if drawn {
                Outcome::Drawn { adapted: false }
            } else {
                Outcome::KeptSource
            };
            crate::diagram::settled(&source, outcome);
        }
        let Some(art) = shown else {
            out.push(block);
            continue;
        };
        if !render.streaming && !art.warnings.is_empty() {
            // TS appends `\n<notice>  \n` to the fence's raw text. A fence followed directly
            // by more text owns that newline in its raw, so a blank line separates the
            // notice; otherwise the notice sits right under the fence.
            let directly_followed = blocks.peek().is_some_and(|next| !next.sep_blank);
            out.push(block);
            out.push(Block {
                kind: BlockKind::ArtParagraph {
                    rows: vec![ArtRow::Warning {
                        text: warning_text(&art.warnings),
                        trailing: "  ".to_owned(),
                    }],
                },
                sep_blank: directly_followed,
                lines: Vec::new(),
            });
        } else {
            let rows = art.rows.iter().cloned().map(ArtRow::Art).collect();
            push_rows(&mut out, block.sep_blank, rows);
        }
        rows_open = true;
    }
    out
}

/// Rewrite the parsed blocks with an installed renderer's layouts. Drawn rows replace the
/// fence (joining a paragraph directly above, as the built-in rows do), the notices
/// follow as rows of the same paragraph; a kept fence gains its notices as a paragraph
/// right under it. Either way the paragraph ends there: the next block starts after a
/// blank line, so following text never joins the last row.
pub(super) fn apply_layouts(
    blocks: Vec<Block>,
    width: usize,
    streaming: bool,
    renderer: &dyn DiagramRenderer,
) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    let mut closed = false;
    for mut block in blocks {
        block.sep_blank |= std::mem::take(&mut closed);
        let Some(source) = mermaid_source(&block) else {
            out.push(block);
            continue;
        };
        let layout = renderer.layout(&source, width, streaming);
        if !streaming {
            crate::diagram::settled(&source, Outcome::of(&layout));
        }
        match layout {
            DiagramLayout::Rows { rows, notices, .. } => {
                let rows = rows
                    .into_iter()
                    .map(ArtRow::Drawn)
                    .chain(notices.into_iter().map(ArtRow::Notice))
                    .collect();
                push_rows(&mut out, block.sep_blank, rows);
                closed = true;
            }
            DiagramLayout::Source { notices } if notices.is_empty() => out.push(block),
            DiagramLayout::Source { notices } => {
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

/// One row an installed renderer drew: its roles take the theme slots, its blank runs
/// `blank(i)` (the run's index in the row).
pub(crate) fn drawn_spans(
    spans: &[DiagramSpan],
    palette: &MermaidPalette,
    blank: impl Fn(usize) -> Style,
) -> Line {
    spans
        .iter()
        .enumerate()
        .map(|(i, span)| {
            let slot = match span.role {
                DiagramRole::Border => palette.border,
                DiagramRole::Text => palette.text,
                DiagramRole::Edge => palette.edge,
                DiagramRole::EdgeLabel => palette.edge_label,
                DiagramRole::Title => palette.title,
                DiagramRole::None => blank(i),
            };
            Span::styled(span.text.clone(), slot)
        })
        .collect()
}

/// One row an installed renderer drew, painted like a built-in row: the leading blank run
/// sits in the code color, a later one after a role's color reset.
fn drawn_line(spans: &[DiagramSpan], style: &MarkdownStyle) -> Line {
    if spans.is_empty() {
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

/// One diagram row painted in the theme (TS `styleSpan` inside the row's `mdCode` span):
/// the leading blank run sits in the code color, a later one after a class's color reset.
fn art_line(spans: &[ArtSpan], style: &MarkdownStyle) -> Line {
    let palette = &style.mermaid_palette;
    if spans.is_empty() {
        // A blank diagram row is a no-break space so the row keeps its height.
        return vec![Span::styled("\u{a0}", style.code)];
    }
    spans
        .iter()
        .enumerate()
        .map(|(i, span)| {
            let slot = match span.cls {
                Cls::Border => palette.border,
                Cls::Text => palette.text,
                Cls::Edge => palette.edge,
                Cls::EdgeLabel => palette.edge_label,
                Cls::None if i == 0 => style.code,
                Cls::None => Style::default(),
            };
            Span::styled(span.text.clone(), slot)
        })
        .collect()
}

/// The paragraph's unwrapped rows; paint wraps them and the row count measures the same.
pub(super) fn art_paragraph_lines(rows: &[ArtRow], style: &MarkdownStyle) -> Vec<Line> {
    rows.iter()
        .map(|row| match row {
            ArtRow::Inline(text) => render_inline(text, style),
            ArtRow::Art(spans) => art_line(spans, style),
            ArtRow::Warning { text, trailing } => {
                let mut line = vec![Span::styled(text.clone(), style.mermaid_palette.warning)];
                if !trailing.is_empty() {
                    line.push(Span::styled(trailing.clone(), style.body));
                }
                line
            }
            ArtRow::Drawn(spans) => drawn_line(spans, style),
            ArtRow::Notice(notice) => vec![Span::styled(
                notice.text.clone(),
                notice_style(notice.level, &style.mermaid_palette),
            )],
        })
        .collect()
}
