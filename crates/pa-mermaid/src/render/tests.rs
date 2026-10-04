//! Byte parity with lovely-mermaid 0.3.3: every case in `goldens.json` was rendered by the
//! package itself under node (`scripts/lovely-mermaid-goldens.mjs`); the port must
//! reproduce each row's role runs, width, and warnings exactly.

use super::{diagram_kind, render, render_cached, Art, ArtSpan, DiagramKind, Role};
use serde_json::Value;

const GOLDENS: &str = include_str!("goldens.json");

/// The package's role names (the golden file's vocabulary).
fn role_name(role: Role) -> &'static str {
    match role {
        Role::Border => "border",
        Role::Text => "text",
        Role::Edge => "edge",
        Role::EdgeLabel => "edgeLabel",
        Role::Title => "title",
        Role::None => "none",
    }
}

/// An art in the golden file's JSON shape (`plain` included: the joined runs).
pub(crate) fn art_json(art: &Art) -> Value {
    let rows: Vec<Value> = art
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|span| serde_json::json!([role_name(span.role), span.text]))
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

#[test]
fn every_golden_case_renders_byte_identical_to_lovely_mermaid() {
    let cases: Vec<Value> = serde_json::from_str(GOLDENS).expect("goldens.json parses");
    assert!(cases.len() >= 75, "the corpus is loaded");
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
    let span = |text: &str, role| ArtSpan {
        text: text.to_owned(),
        role,
    };
    let expected = Art {
        rows: vec![
            vec![
                span("┌───────┐", Role::Border),
                span("    ", Role::None),
                span("┌──────┐", Role::Border),
            ],
            vec![
                span("│", Role::Border),
                span(" ", Role::None),
                span("Start", Role::Text),
                span(" ", Role::None),
                span("├", Role::Border),
                span("───▶", Role::Edge),
                span("│", Role::Border),
                span(" ", Role::None),
                span("Done", Role::Text),
                span(" ", Role::None),
                span("│", Role::Border),
            ],
            vec![
                span("└───────┘", Role::Border),
                span("    ", Role::None),
                span("└──────┘", Role::Border),
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

#[test]
fn diagram_kind_reads_the_header_exactly() {
    let cases = [
        ("flowchart TD\n A --> B", Some(DiagramKind::Flowchart)),
        ("graph LR", Some(DiagramKind::Flowchart)),
        ("stateDiagram-v2", Some(DiagramKind::State)),
        ("stateDiagramFoo\n A --> B", None),
        ("classDiagram-v2", Some(DiagramKind::Class)),
        ("erDiagram", Some(DiagramKind::Er)),
        ("sequenceDiagram", Some(DiagramKind::Sequence)),
        ("pie title x", Some(DiagramKind::Pie)),
        ("mindmap", Some(DiagramKind::Mindmap)),
        ("timeline", Some(DiagramKind::Timeline)),
        ("gitGraph:", Some(DiagramKind::GitGraph)),
        ("---\ntitle: t\n---\ngraph TD", Some(DiagramKind::Flowchart)),
        ("gantt\n title x", None),
        ("\u{7}graph TD", Some(DiagramKind::Flowchart)),
    ];
    for (src, expected) in cases {
        assert_eq!(diagram_kind(src), expected, "{src:?}");
    }
}
