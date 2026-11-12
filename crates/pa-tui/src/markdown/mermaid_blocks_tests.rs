//! The Mermaid fence hook against the fork's TS transform (`createMermaidMarkdownTransform`,
//! `a358fd19e`): `diagram_transform_goldens.json` holds, per case, the layout each fence got,
//! the markdown TS rewrote the text into, and the block structure marked lexed from it
//! (`scripts/lovely-mermaid-goldens.mjs`).

use serde_json::Value;

use super::mermaid_blocks::{ArtRow, MermaidMode, MermaidRender, apply_with};
use super::*;
use crate::theme::{ColorMode, Theme, ThemeColor};

const DIAGRAM_GOLDENS: &str = include_str!("diagram_transform_goldens.json");

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

fn style(mode: MermaidMode, streaming: bool) -> MarkdownStyle {
    MarkdownStyle {
        mermaid: Some(MermaidRender { mode, streaming }),
        ..MarkdownStyle::from_theme(&theme())
    }
}

fn row_texts(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.iter().map(|s| s.content.as_str()).collect())
        .collect()
}

/// A list's item text lines in source order, nested levels included (a list
/// block keeps its rows on its items, not on `Block::lines`).
fn list_texts(list: &ListBlock) -> Vec<String> {
    list.items
        .iter()
        .flat_map(|item| &item.parts)
        .flat_map(|part| match part {
            ListPart::Text(text) | ListPart::LazyText(text) => vec![text.clone()],
            ListPart::List(child) => list_texts(child),
        })
        .collect()
}

/// The blocks in marked's vocabulary: `[type, blank line before, row texts]`.
fn block_shape(block: &Block) -> Value {
    let (kind, rows): (&str, Vec<String>) = match &block.kind {
        BlockKind::Paragraph => ("paragraph", block.lines.clone()),
        BlockKind::ArtParagraph { rows } => (
            "paragraph",
            rows.iter()
                .map(|row| match row {
                    ArtRow::Inline(text) => text.clone(),
                    // A blank diagram row is the TS code span's no-break space.
                    ArtRow::Drawn(spans) if spans.is_empty() => "\u{a0}".to_owned(),
                    ArtRow::Drawn(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
                    ArtRow::Notice(notice) => notice.text.clone(),
                })
                .collect(),
        ),
        BlockKind::Code { .. } => ("code", block.lines.clone()),
        BlockKind::Heading { .. } => ("heading", block.lines.clone()),
        BlockKind::List(list) => ("list", list_texts(list)),
        BlockKind::Quote => ("blockquote", block.lines.clone()),
        BlockKind::Hr => ("hr", Vec::new()),
        BlockKind::Table { .. } => ("table", block.lines.clone()),
    };
    serde_json::json!([kind, block.sep_blank, rows])
}

fn mode_of(case: &Value) -> MermaidMode {
    match case["mode"].as_str().expect("mode") {
        "off" => MermaidMode::Off,
        "final" => MermaidMode::Final,
        "streaming" => MermaidMode::Streaming,
        other => panic!("unknown mode {other}"),
    }
}

const FLOW: &str = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
const FLOW_ROWS: [&str; 3] = [
    "┌───────┐    ┌──────┐",
    "│ Start ├───▶│ Done │",
    "└───────┘    └──────┘",
];

/// The diagram renders as written when its width equals the space exactly and turns a
/// quarter one column short (TS kills the `>=` mutant with the same boundary).
#[test]
fn a_diagram_renders_at_exactly_its_width_and_turns_one_column_less() {
    let fits = render_markdown(FLOW, 21, &style(MermaidMode::Streaming, false));
    assert_eq!(row_texts(&fits), FLOW_ROWS.map(String::from).to_vec());
    // One column short, the flowchart is drawn top to bottom, the notice wrapped like any
    // paragraph text.
    let short = render_markdown(FLOW, 20, &style(MermaidMode::Streaming, false));
    assert_eq!(
        row_texts(&short),
        vec![
            " ┌───────┐",
            " │ Start │",
            " └───┬───┘",
            "     │",
            "     ▼",
            " ┌──────┐",
            " │ Done │",
            " └──────┘",
            "Mermaid diagram",
            "drawn top to bottom",
            "to fit 20 columns",
        ]
    );
}

/// `off` never renders; `final` renders only once the message settles; `streaming` both.
#[test]
fn the_mode_decides_streaming_and_settled_rendering() {
    let fence = vec![
        "  flowchart LR".to_string(),
        "    A[Start] --> B[Done]".to_string(),
    ];
    let art = FLOW_ROWS.map(String::from).to_vec();
    let cases = [
        (MermaidMode::Off, /*streaming*/ true, &fence),
        (MermaidMode::Off, false, &fence),
        (MermaidMode::Final, true, &fence),
        (MermaidMode::Final, false, &art),
        (MermaidMode::Streaming, true, &art),
        (MermaidMode::Streaming, false, &art),
    ];
    for (mode, streaming, expected) in cases {
        let rows = render_markdown(FLOW, 80, &style(mode, streaming));
        assert_eq!(
            &row_texts(&rows),
            expected,
            "{mode:?} streaming={streaming}"
        );
    }
    // Without the hook (thinking, user, and panel markdown) the fence stays code.
    let plain = render_markdown(FLOW, 80, &MarkdownStyle::from_theme(&theme()));
    assert_eq!(row_texts(&plain), fence);
}

/// Each class paints in its TS theme slot; the leading blank run keeps the code color and a
/// later one sits after a color reset.
#[test]
fn diagram_rows_paint_each_class_in_its_theme_slot() {
    let theme = theme();
    let md = style(MermaidMode::Streaming, false);
    let rows = render_markdown("```mermaid\nflowchart TD\n  A -->|yes| B\n```", 80, &md);
    let border = theme.fg_style(ThemeColor::BorderMuted);
    let text = theme.fg_style(ThemeColor::Text);
    let edge = theme.fg_style(ThemeColor::Accent);
    let label = theme.fg_style(ThemeColor::Muted);
    let s = |content: &str, style: Style| Span::styled(content, style);
    let gap = Style::default();
    let node_row = |name| {
        vec![
            s(" ", md.code),
            s("│", border),
            s(" ", gap),
            s(name, text),
            s(" ", gap),
            s("│", border),
        ]
    };
    assert_eq!(
        rows,
        vec![
            vec![s(" ", md.code), s("┌───┐", border)],
            node_row("A"),
            vec![s(" ", md.code), s("└─┬─┘", border)],
            vec![s("   ", md.code), s("│", edge)],
            vec![s("   ", md.code), s("▼", edge), s("yes", label)],
            vec![s(" ", md.code), s("┌───┐", border)],
            node_row("B"),
            vec![s(" ", md.code), s("└───┘", border)],
        ]
    );
}

/// A settled diagram that dropped source is drawn with the first warning listed under it;
/// the same source streaming shows the art alone (warnings are noise mid-stream).
#[test]
fn a_settled_diagram_with_warnings_is_drawn_and_says_what_it_dropped() {
    let source = "```mermaid\nflowchart LR\n  A --> B\n  --> C\n  B --> \n  C D E\n```";
    let warning = theme().fg_style(ThemeColor::Warning);
    let settled = render_markdown(source, 120, &style(MermaidMode::Streaming, false));
    let (notice, art) = settled.split_last().expect("rows");
    assert_eq!(
        notice,
        &vec![Span::styled(
            "Mermaid diagram incomplete: dropped, does not start with a node: \"--> C\" (+2 more)",
            warning,
        )]
    );

    let streaming = render_markdown(source, 120, &style(MermaidMode::Streaming, true));
    assert_eq!(row_texts(art), row_texts(&streaming));
    assert_eq!(
        row_texts(&streaming),
        vec![
            "┌───┐    ┌───┐",
            "│ A ├───▶│ B │",
            "└───┘    └───┘",
            "\u{a0}",
            "┌───┐",
            "│ C │",
            "└───┘",
        ]
    );
}

/// The row count measures exactly the rows the paint emits, cold and replayed from the
/// block cache, for every hook case.
#[test]
fn the_row_count_matches_the_painted_rows() {
    let cases: Vec<Value> = serde_json::from_str(DIAGRAM_GOLDENS).expect("goldens parse");
    for case in &cases {
        let text = case["text"].as_str().expect("text");
        let width = case["width"].as_u64().expect("width") as usize;
        let md = style(
            mode_of(case),
            case["streaming"].as_bool().expect("streaming"),
        );
        let mut cache = MarkdownBlockCache::default();
        let painted = render_markdown_tagged(text, width, &md, "", &mut cache);
        assert_eq!(
            markdown_row_count_tagged(text, width, &md, "", &MarkdownBlockCache::default()),
            painted.len(),
            "{}",
            case["name"]
        );
        assert_eq!(
            markdown_row_count_tagged(text, width, &md, "", &cache),
            painted.len(),
            "{} (cached)",
            case["name"]
        );
    }
}

// ------------------------------------------------------------ recorded layouts

pub(crate) mod replay {
    use std::collections::HashMap;

    use super::*;
    use crate::mermaid::{Art, ArtSpan, Layout, Notice, NoticeLevel, Role};

    /// A layout recorded in a golden file (`scripts/lovely-mermaid-goldens.mjs`).
    pub(crate) fn layout_of(value: &Value) -> Layout {
        let notices = value["notices"]
            .as_array()
            .expect("notices")
            .iter()
            .map(|n| Notice {
                level: match n["level"].as_str().expect("level") {
                    "info" => NoticeLevel::Info,
                    "warning" => NoticeLevel::Warning,
                    other => panic!("unknown level {other}"),
                },
                text: n["text"].as_str().expect("text").to_owned(),
            })
            .collect();
        match value["kind"].as_str().expect("kind") {
            "art" => Layout::Art {
                art: Art {
                    rows: value["rows"]
                        .as_array()
                        .expect("rows")
                        .iter()
                        .map(|row| {
                            row.as_array()
                                .expect("row")
                                .iter()
                                .map(|run| ArtSpan {
                                    role: match run[0].as_str().expect("role") {
                                        "border" => Role::Border,
                                        "text" => Role::Text,
                                        "edge" => Role::Edge,
                                        "edgeLabel" => Role::EdgeLabel,
                                        "title" => Role::Title,
                                        "none" => Role::None,
                                        other => panic!("unknown role {other}"),
                                    },
                                    text: run[1].as_str().expect("text").to_owned(),
                                })
                                .collect()
                        })
                        .collect(),
                    width: value["width"].as_u64().expect("width") as usize,
                    warnings: Vec::new(),
                },
                notices,
                rotated: None,
            },
            "source" => Layout::Source { notices },
            other => panic!("unknown layout kind {other}"),
        }
    }

    /// The layouts a golden case recorded, by fence source, width, and streaming state.
    pub(crate) struct Replay(HashMap<(String, usize, bool), Layout>);

    impl Replay {
        pub(crate) fn of(case: &Value) -> Self {
            Self(
                case["fences"]
                    .as_array()
                    .expect("fences")
                    .iter()
                    .map(|f| {
                        (
                            (
                                f["src"].as_str().expect("src").to_owned(),
                                f["width"].as_u64().expect("width") as usize,
                                f["streaming"].as_bool().expect("streaming"),
                            ),
                            layout_of(&f["layout"]),
                        )
                    })
                    .collect(),
            )
        }

        /// The recorded layout of one fence.
        pub(crate) fn layout(
            &self,
            source: &str,
            available_width: usize,
            streaming: bool,
        ) -> Layout {
            self.0
                .get(&(source.to_owned(), available_width, streaming))
                .cloned()
                .unwrap_or_else(|| {
                    panic!("no recorded layout for {source:?} at {available_width} ({streaming})")
                })
        }
    }

    /// Every case's blocks match what marked lexes from the fork's TS-rewritten markdown
    /// (`createMermaidMarkdownTransform`, `a358fd19e`) given the same layouts: drawn rows,
    /// notices, kept fences, and the paragraph closed after them.
    ///
    /// One deliberate divergence stays out of the corpus: TS appends the notices to a kept
    /// fence's raw text, so under a fence that never closed (a settled message cut off
    /// mid-diagram) its re-lex reads them as more code, backticks and all. Here they sit
    /// under the fence as for a closed one (`an_unclosed_kept_fence_keeps_its_notices_under_it`).
    #[test]
    fn fence_rewrite_matches_the_fork_ts_transform() {
        let cases: Vec<Value> = serde_json::from_str(DIAGRAM_GOLDENS).expect("goldens parse");
        assert!(cases.len() >= 30, "the corpus is loaded");
        let mut mismatches = Vec::new();
        for case in &cases {
            let text = case["text"].as_str().expect("text");
            let width = case["width"].as_u64().expect("width") as usize;
            let streaming = case["streaming"].as_bool().expect("streaming");
            let replay = Replay::of(case);
            let blocks = apply_with(
                parse_blocks(text),
                width,
                &style(mode_of(case), streaming),
                &|src: &str, w: usize, s: bool| replay.layout(src, w, s),
            );
            let actual = Value::Array(blocks.iter().map(block_shape).collect());
            if actual != case["blocks"] {
                mismatches.push(format!(
                    "{}\n  expected: {}\n  actual:   {actual}",
                    case["name"], case["blocks"]
                ));
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    #[test]
    fn an_unclosed_kept_fence_keeps_its_notices_under_it() {
        let kept = |_: &str, _: usize, _: bool| Layout::Source {
            notices: vec![Notice {
                level: NoticeLevel::Warning,
                text: "Mermaid diagram not drawn: no statement could be parsed".to_owned(),
            }],
        };
        let blocks = apply_with(
            parse_blocks("Look:\n\n```mermaid\nflowchart TD\n  ((( "),
            80,
            &style(MermaidMode::Streaming, false),
            &kept,
        );
        assert_eq!(
            Value::Array(blocks.iter().map(block_shape).collect()),
            serde_json::json!([
                ["paragraph", false, ["Look:"]],
                ["code", true, ["flowchart TD", "  ((( "]],
                [
                    "paragraph",
                    false,
                    ["Mermaid diagram not drawn: no statement could be parsed"]
                ],
            ])
        );
    }

    /// A stub layout: one fixed row and one notice for any fence.
    fn stub(source: &str, available_width: usize, streaming: bool) -> Layout {
        Layout::Art {
            art: Art {
                rows: vec![vec![ArtSpan {
                    text: format!("[{}|{available_width}|{streaming}]", source.len()),
                    role: Role::Edge,
                }]],
                width: 1,
                warnings: Vec::new(),
            },
            notices: vec![Notice {
                level: NoticeLevel::Info,
                text: "note".to_owned(),
            }],
            rotated: None,
        }
    }

    #[test]
    fn drawn_rows_and_notices_close_the_paragraph() {
        let md = style(MermaidMode::Streaming, true);
        let rows = row_texts(&render_markdown_with(
            "Before\n```mermaid\nflowchart LR\n```\nAfter",
            40,
            &md,
            &stub,
        ));
        assert_eq!(rows, ["Before", "[12|40|true]", "note", "", "After"]);
    }

    #[test]
    fn the_mode_gates_the_layout() {
        let text = "```mermaid\nflowchart LR\n```";
        let off = row_texts(&render_markdown_with(
            text,
            40,
            &style(MermaidMode::Off, false),
            &stub,
        ));
        let final_streaming = row_texts(&render_markdown_with(
            text,
            40,
            &style(MermaidMode::Final, true),
            &stub,
        ));
        assert_eq!(off, ["  flowchart LR"]);
        assert_eq!(final_streaming, off);
    }

    /// Render through the hook with the fences laid out by `layout`.
    fn render_markdown_with(
        text: &str,
        width: usize,
        style: &MarkdownStyle,
        layout: &dyn Fn(&str, usize, bool) -> Layout,
    ) -> Vec<Line> {
        let blocks = apply_with(parse_blocks(text), width, style, layout);
        let mut lines = Vec::new();
        for (i, block) in blocks.iter().enumerate() {
            if block.sep_blank {
                lines.push(Vec::new());
            }
            render_block(block, blocks.get(i + 1), width, style, &mut lines);
        }
        lines
    }
}
