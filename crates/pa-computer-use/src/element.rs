//! The observation data model every backend produces.
//!
//! An [`Observation`] is one snapshot of an app's focused window: the
//! window's descendants as an [`Element`] tree (the window itself is not
//! indexed), one live element handle per tree element in depth-first walk
//! order, and the window's geometry when known.

use crate::secure::is_secure_field;

/// The per-attribute cap: a hostile app cannot flood the kernel or the
/// model's context with megabyte attribute payloads.
pub const MAX_ATTRIBUTE_CHARS: usize = 2000;
const ELLIPSIS: char = '…';
/// The walk bounds every backend shares.
pub const MAX_DEPTH: usize = 12;
pub const MAX_ELEMENTS: usize = 1500;
/// A hostile element exposing thousands of actions keeps only the first 16.
pub const MAX_ACTIONS: usize = 16;

/// Cap one attribute string at [`MAX_ATTRIBUTE_CHARS`] characters plus an ellipsis.
#[must_use]
pub fn cap(text: Option<String>) -> Option<String> {
    text.map(|text| match text.char_indices().nth(MAX_ATTRIBUTE_CHARS) {
        Some((end, _)) => {
            let mut capped = text[..end].to_string();
            capped.push(ELLIPSIS);
            capped
        }
        None => text,
    })
}

/// An (x, y) pair: a position or a size.
pub type Pair = (f64, f64);

/// A window rect: (x, y, width, height).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    #[must_use]
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One accessibility element in the contract shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub role: Option<String>,
    pub subrole: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    pub description: Option<String>,
    pub placeholder: Option<String>,
    pub actions: Vec<String>,
    pub position: Option<Pair>,
    pub size: Option<Pair>,
    pub children: Vec<Element>,
}

impl Element {
    /// Whether this element is a secure text field (its value never renders).
    #[must_use]
    pub fn is_secure(&self) -> bool {
        is_secure_field(self.role.as_deref(), self.subrole.as_deref())
    }
}

/// One snapshot of an app's focused window.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation<R> {
    pub window_title: Option<String>,
    pub tree: Vec<Element>,
    /// One live handle per tree element, in [`flatten`] order.
    pub refs: Vec<R>,
    /// The window's (x, y, width, height) when known; macOS reports it in
    /// screen space, Wayland in the compositor's logical space.
    pub window_rect: Option<Rect>,
    pub focused_index: Option<usize>,
    /// The window's platform id (the `CGWindowID` on macOS) when readable.
    pub window_id: Option<i64>,
    /// The walk stopped at one of its element, depth or time bounds.
    pub truncated: bool,
}

impl<R> Observation<R> {
    /// The snapshot of a window-less app: nothing to observe yet.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            window_title: None,
            tree: Vec::new(),
            refs: Vec::new(),
            window_rect: None,
            focused_index: None,
            window_id: None,
            truncated: false,
        }
    }
}

/// The tree depth-first, in element-index order.
#[must_use]
pub fn flatten(tree: &[Element]) -> Vec<&Element> {
    let mut flat = Vec::new();
    let mut stack: Vec<&Element> = tree.iter().rev().collect();
    while let Some(element) = stack.pop() {
        flat.push(element);
        stack.extend(element.children.iter().rev());
    }
    flat
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_keeps_2000_characters_plus_an_ellipsis() {
        let capped = cap(Some("x".repeat(5000))).unwrap();
        assert_eq!(capped.chars().count(), 2001);
        assert!(capped.ends_with('…'));
        assert_eq!(cap(Some("short".to_string())), Some("short".to_string()));
        assert_eq!(cap(None), None);
    }

    #[test]
    fn flatten_walks_depth_first_in_index_order() {
        let leaf = |title: &str| Element {
            title: Some(title.to_string()),
            ..Element::default()
        };
        let tree = vec![
            Element {
                children: vec![leaf("a1"), leaf("a2")],
                ..leaf("a")
            },
            leaf("b"),
        ];
        let titles: Vec<_> = flatten(&tree)
            .into_iter()
            .map(|element| element.title.as_deref().unwrap())
            .collect();
        assert_eq!(titles, ["a", "a1", "a2", "b"]);
    }
}
