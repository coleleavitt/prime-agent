//! Ported from the skill's `tests/test_diff.py`, plus byte-exact goldens of
//! the Python renderer's output.

use super::*;
use crate::element::Element;
use crate::testing::{
    element, large_tree, small_tree, with_added_child, with_changed_value, without_child,
};

fn marked(text: &str, marker: char) -> Vec<&str> {
    text.lines()
        .filter(|line| line.starts_with(marker))
        .collect()
}

#[test]
fn the_small_tree_renders_byte_identical_to_the_python_skill() {
    // diff._serialize(fakes.small_tree()["children"]) under CPython 3.11.
    assert_eq!(
        serialize(&small_tree()),
        [
            "[0] AXStaticText 'Label' = 'Hello' @ (20, 60) 200x20",
            "[1] AXTextField 'Search' = 'query' placeholder='Search…' (actions: AXSetValue) @ (20, 90) 240x24",
            "[2] AXButton 'Save' (actions: AXPress) @ (280, 88) 80x28",
            "[3] AXCheckBox 'Enabled' = '1' (actions: AXPress) @ (20, 130) 120x20",
            "[4] AXTextField (AXSecureTextField) 'Password' [secure] (actions: AXSetValue) @ (20, 160) 180x24",
        ]
    );
}

#[test]
fn fractional_geometry_rounds_to_one_decimal_and_empty_attributes_drop() {
    let tree = vec![Element {
        role: None,
        title: Some(String::new()),
        value: Some(String::new()),
        description: Some("it's \"x\"".to_string()),
        position: Some((10.25, 3.0)),
        size: None,
        children: vec![Element {
            role: Some(String::new()),
            position: Some((0.35, -2.5)),
            size: Some((1e16, 0.0)),
            ..Element::default()
        }],
        ..Element::default()
    }];
    // CPython: round(10.25, 1) == 10.2, round(0.35, 1) == 0.3.
    assert_eq!(
        serialize(&tree),
        [
            "[0] AXUnknown description='it\\'s \"x\"' @ (10.2, 3)",
            "  [1] AXUnknown @ (0.3, -2.5) 10000000000000000x0",
        ]
    );
}

#[test]
fn a_shifted_unchanged_element_renders_with_its_new_index() {
    let previous = serialize(&[
        element("AXStaticText", Some("A"), Some("alpha")),
        element("AXStaticText", Some("B"), Some("beta")),
        element("AXStaticText", Some("C"), Some("gamma")),
    ]);
    let shifted = serialize(&[
        element("AXStaticText", Some("B"), Some("beta")),
        element("AXStaticText", Some("C"), Some("gamma")),
        element("AXButton", Some("New"), None),
    ]);
    assert_eq!(
        diff(&previous, &shifted),
        "-[0] AXStaticText 'A' = 'alpha'\n~[0] AXStaticText 'B' = 'beta'\n~[1] AXStaticText 'C' = \
         'gamma'\n+[2] AXButton 'New'"
    );
}

#[test]
fn serialization_is_stable_and_indices_follow_the_walk() {
    let rendered = serialize(&small_tree());
    assert_eq!(rendered, serialize(&small_tree()));
    for (index, line) in rendered.iter().enumerate() {
        assert!(
            line.trim_start().starts_with(&format!("[{index}]")),
            "{line}"
        );
    }
    assert_eq!(serialize(&large_tree()), serialize(&large_tree()));
}

#[test]
fn nested_elements_indent_by_depth() {
    let rendered = serialize(&large_tree());
    assert!(rendered.iter().any(|line| line.starts_with("    [")));
    assert_eq!(rendered.len(), 6 + 6 * 7 + 6 * 7 * 7);
}

#[test]
fn a_secure_field_never_renders_its_value() {
    let text = serialize(&small_tree()).join("\n");
    assert!(!text.contains("hunter2"));
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("Password"))
            .count(),
        1
    );
}

#[test]
fn an_empty_tree_renders_no_lines() {
    assert!(serialize(&[]).is_empty());
}

#[test]
fn identical_snapshots_diff_to_nothing() {
    assert_eq!(
        diff(&serialize(&small_tree()), &serialize(&small_tree())),
        ""
    );
}

#[test]
fn a_changed_value_is_one_tilde_line() {
    let before = serialize(&small_tree());
    let after = serialize(&with_changed_value(small_tree(), "Search", "new query"));
    let output = diff(&before, &after);
    assert_eq!(
        output,
        "~[1] AXTextField 'Search' = 'new query' placeholder='Search…' (actions: AXSetValue) @ \
         (20, 90) 240x24"
    );
    for title in ["Save", "Enabled", "Password"] {
        assert!(!output.contains(title));
    }
}

#[test]
fn an_added_element_is_one_plus_line_and_shifted_followers_surface() {
    let before = serialize(&small_tree());
    let after = serialize(&with_added_child(
        small_tree(),
        "Enabled",
        element("AXButton", Some("Fresh"), None),
    ));
    let output = diff(&before, &after);
    assert_eq!(marked(&output, '+'), ["+  [4] AXButton 'Fresh'"]);
    assert_eq!(marked(&output, '~').len(), 1, "{output}");
}

#[test]
fn a_removed_element_is_one_minus_line() {
    let before = serialize(&small_tree());
    let after = serialize(&without_child(small_tree(), "Password"));
    let output = diff(&before, &after);
    assert_eq!(marked(&output, '-').len(), 1);
    assert!(output.contains("Password"));
    assert!(!output.contains("Save"));
}

#[test]
fn empty_before_and_after_snapshots_show_everything() {
    let current = serialize(&small_tree());
    assert_eq!(marked(&diff(&[], &current), '+').len(), current.len());
    assert_eq!(marked(&diff(&current, &[]), '-').len(), current.len());
}

#[test]
fn a_single_change_in_a_large_tree_diffs_to_one_line() {
    let before = serialize(&large_tree());
    let after = serialize(&with_changed_value(large_tree(), "Text 3-4-2", "renamed"));
    let output = diff(&before, &after);
    assert_eq!(output.lines().count(), 1, "{output}");
}

#[test]
fn equal_content_at_another_depth_does_not_pair() {
    // The indent is part of the compared content, as in the Python regex.
    let before = serialize(&[element("AXGroup", Some("g"), None)]);
    let after = serialize(&[Element {
        children: vec![element("AXGroup", Some("g"), None)],
        ..element("AXWindow", None, None)
    }]);
    assert_eq!(
        diff(&before, &after),
        "-[0] AXGroup 'g'\n+[0] AXWindow\n+  [1] AXGroup 'g'"
    );
}
