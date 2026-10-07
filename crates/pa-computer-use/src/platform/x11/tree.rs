//! `xwininfo -root -tree -int` parsing.
//!
//! Child lines print the id, the raw (unescaped) window name, the `WM_CLASS`
//! pair, a parent-relative geometry and a root-relative absolute position.
//! The parse anchors on the trailing geometry, then the trailing `WM_CLASS`
//! group, then the leading id, so names containing quotes, colons or
//! class-shaped fragments stay intact. Header, root, parent and
//! children-count lines are skipped.

use std::sync::LazyLock;

use regex::Regex;

const MIN_INDENT: usize = 5;
const INDENT_STEP: usize = 3;

/// One parsed tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Window {
    pub id: i64,
    pub depth: usize,
    pub title: Option<String>,
    pub instance: Option<String>,
    pub wm_class: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub rel_x: Option<i64>,
    pub rel_y: Option<i64>,
    pub abs_x: Option<i64>,
    pub abs_y: Option<i64>,
}

static GEOMETRY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\s+(?P<width>\d+)x(?P<height>\d+)(?P<rel_x>[+-]\d+)(?P<rel_y>[+-]\d+)(?:\s+(?P<abs_x>[+-]\d+)(?P<abs_y>[+-]\d+))?\s*$",
    )
    .expect("the geometry pattern compiles")
});
static CLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#": (?P<group>\((?:"(?P<instance>[^"]*)"|\(none\)) ?(?:"(?P<res_class>[^"]*)"|\(none\))?\)|\(\))\s*$"#,
    )
    .expect("the class pattern compiles")
});
static ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<indent>[ ]+)(?P<id>\d+)(?: \(the root window\))?(?P<namepart> .*)?$")
        .expect("the id pattern compiles")
});
static COUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[ ]+\d+ (child|children)[:.]$").expect("the count pattern compiles")
});

fn number(captures: &regex::Captures<'_>, name: &str) -> Option<i64> {
    captures.name(name)?.as_str().parse().ok()
}

/// Parse the tree output into window nodes in depth-first order.
pub(crate) fn parse_tree(output: &str) -> Vec<Window> {
    let mut windows = Vec::new();
    for line in output.lines() {
        if COUNT.is_match(line) {
            continue;
        }
        let geometry = GEOMETRY.captures(line);
        let mut head = match &geometry {
            Some(found) => {
                line[..found.get(0).map_or(line.len(), |whole| whole.start())].trim_end()
            }
            None => line.trim_end(),
        };
        let mut instance = None;
        let mut wm_class = None;
        if let Some(found) = CLASS.captures(head) {
            instance = found
                .name("instance")
                .map(|value| value.as_str().to_string());
            wm_class = found
                .name("res_class")
                .map(|value| value.as_str().to_string());
            let start = found.get(0).map_or(head.len(), |whole| whole.start());
            head = head[..start].trim_end();
        }
        let Some(identified) = ID.captures(head) else {
            continue;
        };
        let indent = identified
            .name("indent")
            .map_or(0, |indent| indent.as_str().len());
        if indent < MIN_INDENT {
            continue;
        }
        let Some(window_id) = number(&identified, "id") else {
            continue;
        };
        let geometry = geometry.as_ref();
        windows.push(Window {
            id: window_id,
            depth: (indent - MIN_INDENT) / INDENT_STEP,
            title: identified
                .name("namepart")
                .and_then(|part| window_name(part.as_str())),
            instance,
            wm_class,
            width: geometry.and_then(|found| number(found, "width")),
            height: geometry.and_then(|found| number(found, "height")),
            rel_x: geometry.and_then(|found| number(found, "rel_x")),
            rel_y: geometry.and_then(|found| number(found, "rel_y")),
            abs_x: geometry.and_then(|found| number(found, "abs_x")),
            abs_y: geometry.and_then(|found| number(found, "abs_y")),
        });
    }
    windows
}

/// A quoted window title from a name part, or `None` when unreadable.
fn window_name(part: &str) -> Option<String> {
    let trimmed = part.trim();
    (trimmed.chars().count() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"'))
        .then(|| trimmed[1..trimmed.len() - 1].to_string())
}
