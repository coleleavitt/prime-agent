//! Byte parity with grok-mermaid 0.2.3: every case in `goldens.json` was rendered by the
//! package itself under node (`scripts/mermaid-goldens.mjs`); the port must reproduce each
//! row's spans, classes, width, and warnings exactly.

use serde_json::Value;

use super::{Art, ArtSpan, Cls, render, render_cached};

const GOLDENS: &str = include_str!("goldens.json");

/// The package's class names (the golden files' vocabulary).
fn cls_name(cls: Cls) -> &'static str {
    match cls {
        Cls::Border => "border",
        Cls::Text => "text",
        Cls::Edge => "edge",
        Cls::EdgeLabel => "edgeLabel",
        Cls::None => "none",
    }
}

/// An art in the golden file's JSON shape (`plain` included: the joined spans).
fn art_json(art: &Art) -> Value {
    let rows: Vec<Value> = art
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|span| serde_json::json!([cls_name(span.cls), span.text]))
                .collect()
        })
        .collect();
    let plain: Vec<String> = art
        .rows
        .iter()
        .map(|row| row.iter().map(|span| span.text.as_str()).collect())
        .collect();
    serde_json::json!({
        "width": art.width,
        "warnings": art.warnings,
        "rows": rows,
        "plain": plain,
    })
}

fn goldens() -> Vec<Value> {
    serde_json::from_str(GOLDENS).expect("goldens.json parses")
}

#[test]
fn every_golden_case_renders_byte_identical_to_grok_mermaid() {
    let cases = goldens();
    assert!(cases.len() >= 30, "the corpus is loaded");
    let mut mismatches = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let src = case["src"].as_str().expect("src");
        let actual = render(src).as_ref().map_or(Value::Null, art_json);
        if actual != case["art"] {
            mismatches.push(format!(
                "{name}\n  expected: {}\n  actual:   {actual}",
                case["art"]
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn the_cached_render_returns_the_render() {
    let src = "flowchart LR\n  A[Start] --> B[Done]";
    let expected = Art {
        rows: vec![
            vec![
                ArtSpan {
                    text: "┌───────┐".into(),
                    cls: Cls::Border,
                },
                ArtSpan {
                    text: "    ".into(),
                    cls: Cls::None,
                },
                ArtSpan {
                    text: "┌──────┐".into(),
                    cls: Cls::Border,
                },
            ],
            vec![
                ArtSpan {
                    text: "│".into(),
                    cls: Cls::Border,
                },
                ArtSpan {
                    text: " ".into(),
                    cls: Cls::None,
                },
                ArtSpan {
                    text: "Start".into(),
                    cls: Cls::Text,
                },
                ArtSpan {
                    text: " ".into(),
                    cls: Cls::None,
                },
                ArtSpan {
                    text: "├".into(),
                    cls: Cls::Border,
                },
                ArtSpan {
                    text: "───▶".into(),
                    cls: Cls::Edge,
                },
                ArtSpan {
                    text: "│".into(),
                    cls: Cls::Border,
                },
                ArtSpan {
                    text: " ".into(),
                    cls: Cls::None,
                },
                ArtSpan {
                    text: "Done".into(),
                    cls: Cls::Text,
                },
                ArtSpan {
                    text: " ".into(),
                    cls: Cls::None,
                },
                ArtSpan {
                    text: "│".into(),
                    cls: Cls::Border,
                },
            ],
            vec![
                ArtSpan {
                    text: "└───────┘".into(),
                    cls: Cls::Border,
                },
                ArtSpan {
                    text: "    ".into(),
                    cls: Cls::None,
                },
                ArtSpan {
                    text: "└──────┘".into(),
                    cls: Cls::Border,
                },
            ],
        ],
        width: 21,
        warnings: Vec::new(),
    };
    assert_eq!(render(src), Some(expected.clone()));
    assert_eq!(*render_cached(src), Some(expected.clone()));
    // A second lookup is served from the cache.
    assert_eq!(*render_cached(src), Some(expected));
}
