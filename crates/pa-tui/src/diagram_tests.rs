//! The plain-text diagram split against the fork's TS `createMermaidTextRenderer`
//! (`a358fd19e`): `custom_message/diagram_text_goldens.json` holds, per case, the segments
//! TS produced and the layout each fence got (`scripts/lovely-mermaid-goldens.mjs`); a
//! split with those layouts replayed must produce the same segments.

use serde_json::Value;

use super::{TextSegment, text_segments};
use crate::markdown::MermaidMode;

const TEXT_GOLDENS: &str = include_str!("custom_message/diagram_text_goldens.json");

/// Segments in the golden file's shape: prose as one string, rows as plain row texts.
fn segments_json(segments: &[TextSegment]) -> Value {
    segments
        .iter()
        .map(|segment| match segment {
            TextSegment::Text(lines) => {
                let text: Vec<String> = lines
                    .iter()
                    .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
                    .collect();
                serde_json::json!({ "kind": "text", "text": text.join("\n") })
            }
            TextSegment::Rows(rows) => {
                let rows: Vec<String> = rows
                    .iter()
                    .map(|row| row.iter().map(|span| span.text.as_str()).collect())
                    .collect();
                serde_json::json!({ "kind": "rows", "rows": rows })
            }
        })
        .collect()
}

#[test]
fn text_split_matches_the_fork_ts_text_renderer() {
    let cases: Vec<Value> = serde_json::from_str(TEXT_GOLDENS).expect("goldens parse");
    assert!(cases.len() >= 10, "the corpus is loaded");
    let mut mismatches = Vec::new();
    for case in &cases {
        let text = case["text"].as_str().expect("text");
        let width = case["width"].as_u64().expect("width") as usize;
        let mode = MermaidMode::from_setting(case["mode"].as_str().expect("mode"));
        let replay = crate::markdown::test_support::Replay::of(case);
        let actual = if super::mode_active(mode, false) {
            text_segments(text, width, &|src: &str, w: usize, s: bool| {
                replay.layout(src, w, s)
            })
            .map_or(Value::Null, |s| segments_json(&s))
        } else {
            Value::Null
        };
        if actual != case["segments"] {
            mismatches.push(format!(
                "{}\n  expected: {}\n  actual:   {actual}",
                case["name"], case["segments"]
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn a_kept_fence_carries_its_notices_as_warning_lines() {
    use crate::mermaid::{Layout, Notice, NoticeLevel};
    let keep_with_note = |_: &str, _: usize, _: bool| Layout::Source {
        notices: vec![Notice {
            level: NoticeLevel::Warning,
            text: "kept".to_owned(),
        }],
    };
    let segments = text_segments("a\n```mermaid\nx\n```\nb", 40, &keep_with_note);
    assert_eq!(
        segments,
        Some(vec![TextSegment::Text(vec![
            vec![("a".to_owned(), None)],
            vec![("```mermaid".to_owned(), None)],
            vec![("x".to_owned(), None)],
            vec![("```".to_owned(), None)],
            vec![("kept".to_owned(), Some(NoticeLevel::Warning))],
            vec![("b".to_owned(), None)],
        ])])
    );
}
