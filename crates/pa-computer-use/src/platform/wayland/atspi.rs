//! AT-SPI observation, focus and element operations, over an accessible-graph seam.
//!
//! The application is found on the accessibility bus by the niri window's
//! pid, the window's frame by its title. Element positions are
//! window-relative (AT-SPI's WINDOW coordinates: Wayland clients cannot
//! know their global position). A password field (`ROLE_PASSWORD_TEXT`)
//! renders as `password text`, is marked `[secure]`, and its value is never
//! read.

use std::time::{Duration, Instant};

use super::niri::WindowRecord;
use crate::element::{cap, Element, MAX_ACTIONS, MAX_DEPTH, MAX_ELEMENTS};
use crate::pyfmt::repr_float;
use crate::secure::ATSPI_SECURE_ROLE;

/// The states the backend reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // the four AT-SPI states the backend reads
pub(crate) struct States {
    pub showing: bool,
    pub focused: bool,
    pub active: bool,
    pub editable: bool,
}

/// The accessibility bus as the backend reads it. Every call maps a D-Bus
/// or toolkit failure to "unreadable" (`None`, or `Err` with its text).
pub(crate) trait AtSpi: Send + Sync {
    /// One live accessible object.
    type Node: Clone + PartialEq + Send + Sync + 'static;

    /// Whether the bus can be used at all; `Err` names the fix.
    fn available(&self) -> Result<(), String>;
    /// The desktop's children (the applications); `None` when the desktop
    /// does not answer.
    fn applications(&self) -> Option<Vec<Self::Node>>;
    fn process_id(&self, node: &Self::Node) -> Option<i64>;
    fn child_count(&self, node: &Self::Node) -> Option<usize>;
    fn child(&self, node: &Self::Node, index: usize) -> Option<Self::Node>;
    fn states(&self, node: &Self::Node) -> Option<States>;
    /// Whether the node's role is `ROLE_PASSWORD_TEXT`; `None` when unreadable.
    fn is_password(&self, node: &Self::Node) -> Option<bool>;
    fn role_name(&self, node: &Self::Node) -> Option<String>;
    fn name(&self, node: &Self::Node) -> Option<String>;
    fn description(&self, node: &Self::Node) -> Option<String>;
    /// Whether the node exposes the Text interface.
    fn has_text(&self, node: &Self::Node) -> bool;
    fn character_count(&self, node: &Self::Node) -> Option<i64>;
    /// `[start, end)` in characters.
    fn text(&self, node: &Self::Node, start: i64, end: i64) -> Option<String>;
    /// The Value interface's current value, when exposed and readable.
    fn current_value(&self, node: &Self::Node) -> Option<f64>;
    /// The Action interface's action count; `None` without the interface.
    fn action_count(&self, node: &Self::Node) -> Option<usize>;
    fn action_name(&self, node: &Self::Node, index: usize) -> Option<String>;
    fn do_action(&self, node: &Self::Node, index: usize) -> Result<bool, String>;
    /// The WINDOW-relative extents (x, y, width, height).
    fn extents(&self, node: &Self::Node) -> Option<(i32, i32, i32, i32)>;
    /// Component.GrabFocus; `None` when not exposed or it failed.
    fn grab_focus(&self, node: &Self::Node) -> Option<bool>;
    /// Whether the node exposes the `EditableText` interface.
    fn has_editable_text(&self, node: &Self::Node) -> bool;
    fn set_text_contents(&self, node: &Self::Node, value: &str) -> Result<bool, String>;
    fn selection_count(&self, node: &Self::Node) -> Option<i64>;
    fn set_selection(&self, node: &Self::Node, start: i64, end: i64) -> Result<bool, String>;
    fn add_selection(&self, node: &Self::Node, start: i64, end: i64) -> Result<bool, String>;
}

const MAX_TEXT_READ: i64 = 2000;
const FINGERPRINT_VALUE_CHARS: usize = 200;

/// One bounded depth-first walk's results.
pub(crate) struct Walk<N> {
    pub tree: Vec<Element>,
    pub refs: Vec<N>,
    pub focused_index: Option<usize>,
    pub truncated: bool,
}

/// The accessible-graph logic over one bus.
pub(crate) struct Accessibility<A: AtSpi> {
    pub(crate) bus: A,
}

impl<A: AtSpi> Accessibility<A> {
    fn showing(states: Option<States>) -> bool {
        states.is_none_or(|states| states.showing)
    }

    /// The node's children, skipping unreadable ones.
    pub(crate) fn children(&self, node: &A::Node) -> Vec<A::Node> {
        (0..self.bus.child_count(node).unwrap_or(0))
            .filter_map(|index| self.bus.child(node, index))
            .collect()
    }

    fn focused(&self, node: &A::Node) -> bool {
        self.bus.states(node).is_some_and(|states| states.focused)
    }

    /// The application owned by `pid` on the bus.
    pub(crate) fn application(&self, pid: Option<i64>) -> Option<A::Node> {
        self.bus
            .applications()?
            .into_iter()
            .find(|app| self.bus.process_id(app) == pid && pid.is_some())
    }

    /// The app's top-level accessible that is the niri window: the frame
    /// named like the window (the active one among several); without a
    /// name match, the active frame of a focused window, or an app's only frame.
    pub(crate) fn frame(&self, app: &A::Node, window: &WindowRecord) -> Option<A::Node> {
        let frames = self.children(app);
        if frames.is_empty() {
            return None;
        }
        let title = window
            .get("title")
            .and_then(serde_json::Value::as_str)
            .filter(|title| !title.is_empty());
        let named: Vec<&A::Node> = frames
            .iter()
            .filter(|frame| title.is_some() && self.bus.name(frame).as_deref() == title)
            .collect();
        let active: Vec<&A::Node> = frames
            .iter()
            .filter(|frame| self.bus.states(frame).is_some_and(|states| states.active))
            .collect();
        if let [only] = named[..] {
            return Some(only.clone());
        }
        if let Some(first) = named.first() {
            return Some(
                (*named
                    .iter()
                    .find(|frame| active.contains(frame))
                    .unwrap_or(first))
                .clone(),
            );
        }
        let focused_window = window
            .get("is_focused")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if focused_window {
            if let Some(first) = active.first() {
                return Some((*first).clone());
            }
        }
        if let [only] = &frames[..] {
            return Some(only.clone());
        }
        None
    }

    /// The node's text through the Text interface (the first 2000
    /// characters), `""` when empty, `None` without the interface.
    fn text_value(&self, node: &A::Node) -> Option<String> {
        if !self.bus.has_text(node) {
            return None;
        }
        let count = self.bus.character_count(node).unwrap_or(0);
        if count <= 0 {
            return Some(String::new());
        }
        self.bus.text(node, 0, count.min(MAX_TEXT_READ))
    }

    /// One accessible in the element contract.
    pub(crate) fn describe(&self, node: &A::Node) -> Element {
        let secure = self.bus.is_password(node);
        let role = if secure == Some(true) {
            Some(ATSPI_SECURE_ROLE.to_string())
        } else {
            cap(self.bus.role_name(node))
        };
        let title = cap(self.bus.name(node));
        let mut value = None;
        if secure != Some(true) {
            value = cap(self.text_value(node));
            if value.is_some() && value == title {
                value = None; // labels repeat their name through Text: render it once
            }
            if value.is_none() {
                value = self.bus.current_value(node).map(repr_float);
            }
        }
        let actions = (0..self.bus.action_count(node).unwrap_or(0).min(MAX_ACTIONS))
            .filter_map(|index| self.bus.action_name(node, index))
            .filter(|name| !name.is_empty())
            .collect();
        let (position, size) = match self.bus.extents(node) {
            Some((x, y, width, height)) if width > 0 && height > 0 => (
                Some((f64::from(x), f64::from(y))),
                Some((f64::from(width), f64::from(height))),
            ),
            _ => (None, None),
        };
        Element {
            role,
            title,
            value,
            description: cap(self.bus.description(node)).filter(|text| !text.is_empty()),
            actions,
            position,
            size,
            ..Element::default()
        }
    }

    /// Walk the frame's showing descendants depth-first within the element,
    /// depth and time bounds. Hidden elements (closed menus, hidden tabs)
    /// are skipped with their subtrees.
    pub(crate) fn walk(&self, frame: &A::Node, deadline: Instant) -> Walk<A::Node> {
        let mut walk = Walk {
            tree: Vec::new(),
            refs: Vec::new(),
            focused_index: None,
            truncated: false,
        };
        let mut tree = Vec::new();
        self.walk_into(frame, 1, &mut tree, &mut walk, deadline);
        walk.tree = tree;
        walk
    }

    fn walk_into(
        &self,
        parent: &A::Node,
        depth: usize,
        siblings: &mut Vec<Element>,
        walk: &mut Walk<A::Node>,
        deadline: Instant,
    ) {
        for child in self.children(parent) {
            if walk.refs.len() >= MAX_ELEMENTS || Instant::now() > deadline {
                walk.truncated = true;
                return;
            }
            let states = self.bus.states(&child);
            if !Self::showing(states) {
                continue;
            }
            let mut element = self.describe(&child);
            if states.is_some_and(|states| states.focused) {
                walk.focused_index = Some(walk.refs.len());
            }
            walk.refs.push(child.clone());
            if depth < MAX_DEPTH {
                self.walk_into(&child, depth + 1, &mut element.children, walk, deadline);
            } else if self.bus.child_count(&child).unwrap_or(0) > 0 {
                walk.truncated = true;
            }
            siblings.push(element);
        }
    }

    /// The frames focus can live in: the bound frame plus the app's active
    /// frames (dialogs).
    pub(crate) fn focus_roots(&self, app: &A::Node, window: &WindowRecord) -> Vec<A::Node> {
        let mut roots: Vec<A::Node> = self.frame(app, window).into_iter().collect();
        for candidate in self.children(app) {
            if !roots.contains(&candidate)
                && self
                    .bus
                    .states(&candidate)
                    .is_some_and(|states| states.active)
            {
                roots.push(candidate);
            }
        }
        roots
    }

    /// Search the roots' showing descendants for the focused element:
    /// `(element, complete)`, incomplete when a bound stopped the search.
    pub(crate) fn find_focused(
        &self,
        roots: &[A::Node],
        deadline: Instant,
    ) -> (Option<A::Node>, bool) {
        let mut stack: Vec<A::Node> = roots.iter().rev().cloned().collect();
        let mut seen = 0;
        while let Some(node) = stack.pop() {
            if seen >= MAX_ELEMENTS || Instant::now() > deadline {
                return (None, false);
            }
            seen += 1;
            let states = self.bus.states(&node);
            if !Self::showing(states) {
                continue;
            }
            if states.is_some_and(|states| states.focused) {
                return (Some(node), true);
            }
            let mut children = self.children(&node);
            children.reverse();
            stack.extend(children);
        }
        (None, true)
    }

    /// The live (role, name) as rendered; unreadable reads `(None, None)`.
    pub(crate) fn live_fingerprint(&self, node: &A::Node) -> (Option<String>, Option<String>) {
        if self.bus.available().is_err() {
            return (None, None);
        }
        let Some(secure) = self.bus.is_password(node) else {
            return (None, None);
        };
        let role = if secure {
            Some(ATSPI_SECURE_ROLE.to_string())
        } else {
            cap(self.bus.role_name(node))
        };
        (role, cap(self.bus.name(node)))
    }

    /// The fingerprint tail: the first root's child count and the focused
    /// element's role, name and (only when verifiably not secure) text head.
    pub(crate) fn focus_fingerprint(
        &self,
        roots: &[A::Node],
        budget: Duration,
    ) -> [Option<String>; 4] {
        let count = roots
            .first()
            .and_then(|root| self.bus.child_count(root))
            .map(|count| count.to_string());
        let (focused, _) = self.find_focused(roots, Instant::now() + budget);
        let Some(focused) = focused else {
            return [count, None, None, None];
        };
        let head = if self.bus.is_password(&focused) == Some(false) {
            self.text_value(&focused)
                .map(|text| text.chars().take(FINGERPRINT_VALUE_CHARS).collect())
        } else {
            Some(String::new()) // an unverifiable or secure field's value is never read
        };
        [
            count,
            self.bus.role_name(&focused),
            self.bus.name(&focused),
            head,
        ]
    }

    /// The element's full current text (the uncapped search source).
    pub(crate) fn current_value(&self, node: &A::Node) -> Option<String> {
        if !self.bus.has_text(node) {
            return None;
        }
        let count = self.bus.character_count(node).unwrap_or(0);
        if count <= 0 {
            return Some(String::new());
        }
        self.bus.text(node, 0, count)
    }

    /// Whether the node accepts text writes: `EditableText` plus `EDITABLE`.
    pub(crate) fn is_settable(&self, node: &A::Node) -> bool {
        self.bus.has_editable_text(node)
            && self.bus.states(node).is_some_and(|states| states.editable)
    }

    /// Poll `STATE_FOCUSED` until it holds or `timeout` passes.
    pub(crate) fn wait_focused(&self, node: &A::Node, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.focused(node) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
