//! Mermaid diagrams through the binary's composition: the TUI's markdown hook draws
//! lovely-mermaid 0.3.3 rows in the fork build (`feature = "mermaid"`, on by default) and
//! grok-mermaid 0.2.3 rows in the native build (`--no-default-features`), each byte for
//! byte against the goldens its renderer was generated with.

use pa_tui::markdown::{MarkdownStyle, MermaidMode, render_markdown};
use serde_json::Value;

/// The rows the assistant-text markdown draws for one `mermaid` fence, as plain text (a
/// blank diagram row is the no-break space that keeps its height); `streaming` messages
/// carry no notices.
fn drawn_rows(src: &str, width: usize, streaming: bool) -> Vec<String> {
    pa_cli::features::install_tui_features();
    let style = MarkdownStyle::default().with_mermaid(MermaidMode::Streaming, streaming);
    render_markdown(&format!("```mermaid\n{src}\n```"), width, &style)
        .iter()
        .map(|line| {
            let row: String = line.iter().map(|span| span.content.as_str()).collect();
            if row == "\u{a0}" { String::new() } else { row }
        })
        .collect()
}

/// `(name, src, plain rows)` of every golden case that draws.
fn drawn_cases(goldens: &str) -> Vec<(String, String, Vec<String>)> {
    let cases: Vec<Value> = serde_json::from_str(goldens).expect("goldens parse");
    cases
        .iter()
        .filter(|case| case["art"].is_object())
        .map(|case| {
            let plain = case["art"]["plain"]
                .as_array()
                .expect("plain")
                .iter()
                .map(|row| row.as_str().expect("row").to_owned())
                .collect();
            (
                case["name"].as_str().expect("name").to_owned(),
                case["src"].as_str().expect("src").to_owned(),
                plain,
            )
        })
        .collect()
}

#[cfg(feature = "mermaid")]
mod fork {
    use super::*;

    const LOVELY_GOLDENS: &str = include_str!("../../pa-mermaid/src/render/goldens.json");

    /// Every lovely-mermaid case that draws renders its golden rows through the installed
    /// renderer at the TUI's own widths (streaming, so no notice follows). The tab case is
    /// the one the transcript never feeds the renderer: markdown expands tabs before the
    /// hook runs.
    #[test]
    fn the_fork_build_draws_lovely_mermaid_rows() {
        let cases = drawn_cases(LOVELY_GOLDENS);
        assert!(cases.len() >= 70, "the corpus is loaded");
        let mut mismatches = Vec::new();
        for (name, src, plain) in cases {
            if src.contains('\t') {
                continue;
            }
            let rows = drawn_rows(&src, 2000, true);
            if rows != plain {
                mismatches.push(format!(
                    "{name}\n  expected: {plain:?}\n  actual:   {rows:?}"
                ));
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    /// Settled art with warnings is drawn, its first warning listed under it.
    #[test]
    fn settled_art_lists_its_warnings_under_it() {
        let rows = drawn_rows("flowchart LR\n  A --> B\n  --> C", 80, false);
        assert_eq!(
            rows,
            [
                "┌───┐    ┌───┐",
                "│ A ├───▶│ B │",
                "└───┘    └───┘",
                "Mermaid diagram incomplete: dropped, does not start with a node: \"--> C\"",
            ]
        );
    }

    /// The wide side-by-side flowchart is redrawn left to right at a width the top-down
    /// layout overflows, and keeps its source with the columns it needs below that.
    #[test]
    fn a_too_wide_flowchart_turns_a_quarter_or_says_what_it_needs() {
        let cases: Vec<Value> = serde_json::from_str(LOVELY_GOLDENS).expect("goldens parse");
        let case = |name: &str| {
            cases
                .iter()
                .find(|case| case["name"] == name)
                .unwrap_or_else(|| panic!("{name}"))
                .clone()
        };
        let td = case("axis_flip_side_by_side_td");
        let lr = case("axis_flip_side_by_side_lr");
        let src = td["src"].as_str().expect("src");
        let lr_width = usize::try_from(lr["art"]["width"].as_u64().expect("width")).expect("fits");
        let expected: Vec<String> = lr["art"]["plain"]
            .as_array()
            .expect("plain")
            .iter()
            .map(|row| row.as_str().expect("row").to_owned())
            .collect();
        let rows = drawn_rows(src, lr_width, false);
        let (art, notice) = rows.split_at(expected.len());
        assert_eq!(art, expected.as_slice());
        // The notice wraps at the transcript width like any paragraph text.
        assert_eq!(
            notice.join(" "),
            format!("Mermaid diagram drawn left to right to fit {lr_width} columns")
        );

        let narrow = drawn_rows(src, lr_width - 1, false);
        assert_eq!(narrow.first().map(String::as_str), Some("  flowchart TD"));
        let fence_end = narrow
            .iter()
            .rposition(|row| row.starts_with("      end"))
            .expect("the kept source");
        // A blank row separates the code block from the paragraph under it.
        assert_eq!(narrow[fence_end + 1], "");
        assert_eq!(
            narrow[fence_end + 2..].join(" "),
            format!(
                "Mermaid diagram not drawn: needs {lr_width} columns, {} available",
                lr_width - 1
            )
        );
    }
}

#[cfg(not(feature = "mermaid"))]
mod native {
    use super::*;

    const GROK_GOLDENS: &str = include_str!("../../pa-tui/src/mermaid/goldens.json");

    /// Without the feature no renderer is installed: every grok-mermaid 0.2.3 case that
    /// draws renders exactly its golden rows, as the native product does.
    #[test]
    fn the_native_build_draws_grok_mermaid_rows() {
        let cases = drawn_cases(GROK_GOLDENS);
        assert!(cases.len() >= 30, "the corpus is loaded");
        let mut mismatches = Vec::new();
        for (name, src, plain) in cases {
            let rows = drawn_rows(&src, 2000, true);
            if rows != plain {
                mismatches.push(format!(
                    "{name}\n  expected: {plain:?}\n  actual:   {rows:?}"
                ));
            }
        }
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    }

    /// A diagram wider than the transcript keeps its fence (wrapped like any code), with
    /// no note: the native transform has no axis flip.
    #[test]
    fn the_native_build_keeps_a_too_wide_fence_silently() {
        let src = "flowchart LR\n  A[Start] --> B[Done]";
        assert_eq!(
            drawn_rows(src, 20, false),
            ["  flowchart LR", "    A[Start] -->", "B[Done]"]
        );
    }
}
