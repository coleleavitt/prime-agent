//! The fork's diagram policy against the TS `layoutMermaid` (`a358fd19e`): every case in
//! `policy_goldens.json` was laid out by the TS function itself, verbatim, over
//! lovely-mermaid 0.3.3 under node (`scripts/lovely-mermaid-goldens.mjs`). The unit cases
//! port `test/mermaid.test.ts`'s `rotateFlowchart` and `layoutMermaid` blocks.

use serde_json::{Value, json};

use super::{Axis, Layout, Notice, NoticeLevel, Rotated, layout, rotate_flowchart};
use crate::render::tests::art_json;

const POLICY_GOLDENS: &str = include_str!("policy_goldens.json");

fn notices_json(notices: &[Notice]) -> Value {
    notices
        .iter()
        .map(|n| {
            let level = match n.level {
                NoticeLevel::Info => "info",
                NoticeLevel::Warning => "warning",
            };
            json!({ "level": level, "text": n.text })
        })
        .collect()
}

/// A layout in the golden file's JSON shape.
fn layout_json(layout: &Layout) -> Value {
    match layout {
        Layout::Art { art, notices, .. } => {
            let art = art_json(art);
            json!({ "kind": "art", "rows": art["rows"], "width": art["width"], "notices": notices_json(notices) })
        }
        Layout::Source { notices } => json!({ "kind": "source", "notices": notices_json(notices) }),
    }
}

#[test]
fn every_policy_case_matches_the_ts_layout() {
    let cases: Vec<Value> = serde_json::from_str(POLICY_GOLDENS).expect("policy goldens parse");
    assert!(cases.len() >= 15, "the corpus is loaded");
    let mut mismatches = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let src = case["src"].as_str().expect("src");
        let width = case["width"].as_u64().expect("width") as usize;
        let streaming = case["streaming"].as_bool().expect("streaming");
        let actual = layout_json(&layout(src, width, streaming));
        if actual != case["layout"] {
            mismatches.push(format!(
                "{name}\n  expected: {}\n  actual:   {actual}",
                case["layout"]
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}

#[test]
fn turns_vertical_flowcharts_left_to_right_and_horizontal_ones_top_to_bottom() {
    assert_eq!(
        rotate_flowchart("flowchart TD\n  A --> B"),
        Some(Rotated {
            source: "flowchart LR\n  A --> B".to_owned(),
            axis: Axis::LeftToRight,
        })
    );
    assert_eq!(
        rotate_flowchart("graph BT;A-->B"),
        Some(Rotated {
            source: "graph LR;A-->B".to_owned(),
            axis: Axis::LeftToRight,
        })
    );
    assert_eq!(
        rotate_flowchart("flowchart RL\nA-->B"),
        Some(Rotated {
            source: "flowchart TD\nA-->B".to_owned(),
            axis: Axis::TopToBottom,
        })
    );
    assert_eq!(
        rotate_flowchart("flowchart\nA-->B"),
        Some(Rotated {
            source: "flowchart LR\nA-->B".to_owned(),
            axis: Axis::LeftToRight,
        })
    );
}

#[test]
fn rotation_skips_frontmatter_blank_lines_and_comments_before_the_header() {
    let source = "\n---\ntitle: T\n---\n%% note\nflowchart TB %% trailing\nA-->B";
    assert_eq!(
        rotate_flowchart(source),
        Some(Rotated {
            source: "\n---\ntitle: T\n---\n%% note\nflowchart LR %% trailing\nA-->B".to_owned(),
            axis: Axis::LeftToRight,
        })
    );
}

#[test]
fn rotation_matches_the_header_like_the_ts_regex() {
    // Case-insensitive keyword and direction; an unknown direction word backtracks to the
    // bare keyword (the TS lookahead accepts the space after it).
    assert_eq!(
        [
            rotate_flowchart("Graph lr\nA-->B").map(|r| r.source),
            rotate_flowchart("  graph TDX\nA").map(|r| r.source),
            rotate_flowchart("graph\tTD;A").map(|r| r.source),
        ],
        [
            Some("Graph TD\nA-->B".to_owned()),
            Some("  graph LR TDX\nA".to_owned()),
            Some("graph LR;A".to_owned()),
        ]
    );
}

#[test]
fn rotation_leaves_other_diagram_types_alone() {
    assert_eq!(rotate_flowchart("sequenceDiagram\nA->>B: hi"), None);
    assert_eq!(rotate_flowchart("flowcharts TD\nA-->B"), None);
    assert_eq!(rotate_flowchart(""), None);
}

const SMALL: &str = "flowchart LR\n  A[Start] --> B[Done]";

/// Three unconnected subgraphs side by side top-to-bottom: wide as TD, narrow rotated.
fn side_by_side() -> String {
    let mut lines = vec!["flowchart TD".to_owned()];
    for name in ["one", "two", "three"] {
        lines.push(format!(
            "    subgraph {}[\"tier {name}\"]",
            name.to_uppercase()
        ));
        for part in ["a", "b", "c"] {
            lines.push(format!(
                "        {name}{}[\"a fairly long label for {name} {part}\"]",
                part.to_uppercase()
            ));
        }
        lines.push("    end".to_owned());
    }
    lines.join("\n")
}

fn art_of(layout: Layout) -> crate::Art {
    match layout {
        Layout::Art { art, .. } => art,
        Layout::Source { notices } => panic!("expected art, got source with {notices:?}"),
    }
}

#[test]
fn draws_a_diagram_that_fits() {
    let Layout::Art {
        notices, rotated, ..
    } = layout(SMALL, 80, false)
    else {
        panic!("expected art");
    };
    assert_eq!((notices, rotated), (Vec::new(), None));
}

#[test]
fn rotates_a_flowchart_that_is_too_wide_and_says_so_once_settled() {
    let source = side_by_side();
    let natural = art_of(layout(&source, 1000, false));
    let rotated = art_of(layout(
        &rotate_flowchart(&source).expect("flowchart").source,
        1000,
        false,
    ));
    assert!(rotated.width < natural.width);

    let width = rotated.width;
    assert_eq!(
        layout(&source, width, false),
        Layout::Art {
            art: rotated.clone(),
            notices: vec![Notice {
                level: NoticeLevel::Info,
                text: format!("Mermaid diagram drawn left to right to fit {width} columns"),
            }],
            rotated: Some(Axis::LeftToRight),
        }
    );
    assert_eq!(
        layout(&source, width, true),
        Layout::Art {
            art: rotated,
            notices: Vec::new(),
            rotated: Some(Axis::LeftToRight),
        }
    );
}

#[test]
fn reports_the_narrowest_width_it_could_manage_when_nothing_fits() {
    let source = side_by_side();
    let rotated = art_of(layout(
        &rotate_flowchart(&source).expect("flowchart").source,
        1000,
        false,
    ));
    let width = rotated.width - 1;
    assert_eq!(
        layout(&source, width, false),
        Layout::Source {
            notices: vec![Notice {
                level: NoticeLevel::Warning,
                text: format!(
                    "Mermaid diagram not drawn: needs {} columns, {width} available",
                    rotated.width
                ),
            }],
        }
    );
    assert_eq!(
        layout(&source, width, true),
        Layout::Source {
            notices: Vec::new()
        }
    );
}

#[test]
fn draws_art_with_warnings_and_lists_them_beside_it() {
    let source = "flowchart LR\n  A --> B\n  this is not ~~~ valid ((( ";
    let Layout::Art { notices, .. } = layout(source, 80, false) else {
        panic!("expected art");
    };
    assert_eq!(
        notices,
        vec![Notice {
            level: NoticeLevel::Warning,
            text: "Mermaid diagram incomplete: dropped, expected a link: \"is not ~~~ valid (((\""
                .to_owned(),
        }]
    );
    let Layout::Art { notices, .. } = layout(source, 80, true) else {
        panic!("expected art");
    };
    assert_eq!(notices, Vec::new());
}

#[test]
fn explains_unsupported_and_unparseable_diagrams() {
    assert_eq!(
        layout("gantt\n  title x", 80, false),
        Layout::Source {
            notices: vec![Notice {
                level: NoticeLevel::Info,
                text: "Mermaid diagram not drawn: gantt is not supported in the terminal"
                    .to_owned(),
            }],
        }
    );
    assert_eq!(
        layout("flowchart TD\n  ((( ", 80, false),
        Layout::Source {
            notices: vec![Notice {
                level: NoticeLevel::Warning,
                text: "Mermaid diagram not drawn: no statement could be parsed".to_owned(),
            }],
        }
    );
}
