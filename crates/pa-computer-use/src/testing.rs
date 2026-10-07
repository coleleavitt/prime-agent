//! Test fixtures shared by the unit tests (the skill's `tests/fakes.py`
//! element builders and settings writer).

use std::path::{Path, PathBuf};

use crate::element::Element;

/// One element with the given role, title and value.
pub(crate) fn element(role: &str, title: Option<&str>, value: Option<&str>) -> Element {
    Element {
        role: Some(role.to_string()),
        title: title.map(ToString::to_string),
        value: value.map(ToString::to_string),
        ..Element::default()
    }
}

/// [`element`] with actions and geometry.
pub(crate) fn placed(
    mut element: Element,
    actions: &[&str],
    position: (f64, f64),
    size: (f64, f64),
) -> Element {
    element.actions = actions.iter().map(ToString::to_string).collect();
    element.position = Some(position);
    element.size = Some(size);
    element
}

/// The small canned window (its children): a label, a search field, a
/// button, a checkbox and a secure password field.
pub(crate) fn small_tree() -> Vec<Element> {
    vec![
        placed(
            element("AXStaticText", Some("Label"), Some("Hello")),
            &[],
            (20.0, 60.0),
            (200.0, 20.0),
        ),
        placed(
            Element {
                placeholder: Some("Search…".to_string()),
                ..element("AXTextField", Some("Search"), Some("query"))
            },
            &["AXSetValue"],
            (20.0, 90.0),
            (240.0, 24.0),
        ),
        placed(
            element("AXButton", Some("Save"), None),
            &["AXPress"],
            (280.0, 88.0),
            (80.0, 28.0),
        ),
        placed(
            element("AXCheckBox", Some("Enabled"), Some("1")),
            &["AXPress"],
            (20.0, 130.0),
            (120.0, 20.0),
        ),
        placed(
            Element {
                subrole: Some("AXSecureTextField".to_string()),
                ..element("AXTextField", Some("Password"), Some("hunter2"))
            },
            &["AXSetValue"],
            (20.0, 160.0),
            (180.0, 24.0),
        ),
    ]
}

/// A deterministic tree of several hundred elements.
pub(crate) fn large_tree() -> Vec<Element> {
    (0..6)
        .map(|g| Element {
            children: (0..7)
                .map(|r| Element {
                    children: (0..7)
                        .map(|i| {
                            element(
                                "AXStaticText",
                                Some(&format!("Text {g}-{r}-{i}")),
                                Some(&format!("v{g}-{r}-{i}")),
                            )
                        })
                        .collect(),
                    ..element("AXGroup", Some(&format!("Group {g} row {r}")), None)
                })
                .collect(),
            ..element("AXGroup", Some(&format!("Group {g}")), None)
        })
        .collect()
}

fn find_mut<'a>(tree: &'a mut [Element], title: &str) -> Option<&'a mut Element> {
    for element in tree {
        if element.title.as_deref() == Some(title) {
            return Some(element);
        }
        if let Some(found) = find_mut(&mut element.children, title) {
            return Some(found);
        }
    }
    None
}

/// A copy with the titled element's value replaced.
pub(crate) fn with_changed_value(mut tree: Vec<Element>, title: &str, value: &str) -> Vec<Element> {
    find_mut(&mut tree, title).expect("titled element").value = Some(value.to_string());
    tree
}

/// A copy with `child` appended under the titled element.
pub(crate) fn with_added_child(
    mut tree: Vec<Element>,
    parent: &str,
    child: Element,
) -> Vec<Element> {
    find_mut(&mut tree, parent)
        .expect("titled parent")
        .children
        .push(child);
    tree
}

/// A copy with the titled element removed from its parent.
pub(crate) fn without_child(mut tree: Vec<Element>, title: &str) -> Vec<Element> {
    fn remove(tree: &mut Vec<Element>, title: &str) -> bool {
        if let Some(index) = tree
            .iter()
            .position(|element| element.title.as_deref() == Some(title))
        {
            tree.remove(index);
            return true;
        }
        tree.iter_mut()
            .any(|element| remove(&mut element.children, title))
    }
    assert!(remove(&mut tree, title), "titled element");
    tree
}

/// Write a `computer-use.toml` fixture into `dir` and return its path.
pub(crate) fn write_settings(
    dir: &Path,
    allowed: &[&str],
    blocked: &[&str],
    system_deny: &[&str],
    risk: &[(&str, &str)],
) -> PathBuf {
    let quoted = |items: &[&str]| {
        items
            .iter()
            .map(|item| format!("\"{item}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut lines = Vec::new();
    if !system_deny.is_empty() {
        lines.push(format!("system_deny = [{}]", quoted(system_deny)));
    }
    if !allowed.is_empty() || !blocked.is_empty() {
        lines.push("[apps]".to_string());
        if !allowed.is_empty() {
            lines.push(format!("allowed = [{}]", quoted(allowed)));
        }
        if !blocked.is_empty() {
            lines.push(format!("blocked = [{}]", quoted(blocked)));
        }
    }
    if !risk.is_empty() {
        lines.push("[risk]".to_string());
        for (bundle_id, label) in risk {
            lines.push(format!("\"{bundle_id}\" = \"{label}\""));
        }
    }
    let path = dir.join("computer-use.toml");
    std::fs::write(&path, lines.join("\n") + "\n").expect("write settings fixture");
    path
}
