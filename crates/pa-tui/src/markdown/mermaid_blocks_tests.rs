//! The Mermaid fence hook against the TS v0.9.8 transform: `mermaid_transform_goldens.json`
//! holds, per case, the markdown TS rewrote the assistant text into and the block structure
//! marked lexed from it (`scripts/mermaid-goldens.mjs`).

use super::mermaid_blocks::{apply, ArtRow, MermaidMode, MermaidRender};
use super::*;
use crate::theme::{ColorMode, Theme, ThemeColor};
use serde_json::Value;

const TRANSFORM_GOLDENS: &str = include_str!("mermaid_transform_goldens.json");

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
                    ArtRow::Art(spans) if spans.is_empty() => "\u{a0}".to_owned(),
                    ArtRow::Art(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
                    ArtRow::Warning { text, trailing } => format!("{text}{trailing}"),
                })
                .collect(),
        ),
        BlockKind::Code { .. } => ("code", block.lines.clone()),
        BlockKind::Heading => ("heading", block.lines.clone()),
        BlockKind::List { .. } => ("list", block.lines.clone()),
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

/// Every case's blocks match what marked lexes from the TS-rewritten markdown: the same
/// diagram rows, the same paragraph joins, the same blank lines, the same kept fences.
#[test]
fn fence_rewrite_matches_the_ts_transform() {
    let cases: Vec<Value> = serde_json::from_str(TRANSFORM_GOLDENS).expect("goldens parse");
    assert!(cases.len() >= 20, "the corpus is loaded");
    let mut mismatches = Vec::new();
    for case in &cases {
        let text = case["text"].as_str().expect("text");
        let width = case["width"].as_u64().expect("width") as usize;
        let streaming = case["streaming"].as_bool().expect("streaming");
        let blocks = apply(parse_blocks(text), width, &style(mode_of(case), streaming));
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

const FLOW: &str = "```mermaid\nflowchart LR\n  A[Start] --> B[Done]\n```";
const FLOW_ROWS: [&str; 3] = [
    "┌───────┐    ┌──────┐",
    "│ Start ├───▶│ Done │",
    "└───────┘    └──────┘",
];

/// The diagram renders when its width equals the space exactly and falls back to the
/// fence one column short (TS kills the `>=` mutant with the same boundary).
#[test]
fn a_diagram_renders_at_exactly_its_width_and_not_one_column_less() {
    let fits = render_markdown(FLOW, 21, &style(MermaidMode::Streaming, false));
    assert_eq!(row_texts(&fits), FLOW_ROWS.map(String::from).to_vec());
    // One column short, the fence renders as code (and wraps like any code row).
    let short = render_markdown(FLOW, 20, &style(MermaidMode::Streaming, false));
    assert_eq!(
        row_texts(&short),
        vec!["  flowchart LR", "    A[Start] -->", "B[Done]"]
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

/// A settled diagram that dropped source keeps its fence and gains the warning notice; the
/// same source streaming shows the partial art (warnings are advisory mid-stream).
#[test]
fn a_settled_diagram_with_warnings_keeps_its_source_and_says_why() {
    let source = "```mermaid\nflowchart LR\n  A --> B\n  --> C\n  B --> \n  C D E\n```";
    let warning = theme().fg_style(ThemeColor::Warning);
    let md = style(MermaidMode::Streaming, false);
    let settled = render_markdown(source, 120, &md);
    let last = settled.last().expect("rows");
    assert_eq!(
        last,
        &vec![
            Span::styled(
                "Mermaid diagram not rendered: dropped, does not start with a node: \"--> C\" (+2 more)",
                warning,
            ),
            Span::styled("  ", md.body),
        ]
    );
    assert_eq!(
        row_texts(&settled[..settled.len() - 2]),
        vec![
            "  flowchart LR",
            "    A --> B",
            "    --> C",
            "    B --> ",
            "    C D E",
        ]
    );
    assert_eq!(settled[settled.len() - 2], Vec::<Span>::new());

    let streaming = render_markdown(source, 120, &style(MermaidMode::Streaming, true));
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
    let cases: Vec<Value> = serde_json::from_str(TRANSFORM_GOLDENS).expect("goldens parse");
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
