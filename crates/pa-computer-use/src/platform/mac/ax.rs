//! The macOS accessibility reads over the raw AX seam ([`Ax`]).
//!
//! Everything the skill's `ax.py` decided lives here, platform-independent:
//! the bounded walk of the focused window, the per-element description with
//! its secure-field rules, the fail-closed live focus and field reads, and
//! the settle fingerprint. [`Ax`] is only the attribute transport, so the
//! test doubles below it exercise every rule on any host.

use std::time::{Duration, Instant};

use serde_json::json;

use crate::element::{Element, MAX_ACTIONS, MAX_DEPTH, MAX_ELEMENTS, Observation, Pair, Rect, cap};
use crate::error::{ComputerUseError, Result, head, unsupported};
use crate::platform::Fingerprint;
use crate::pyfmt::repr_float;
use crate::secure::{MAC_SECURE_ROLE, MAC_SECURE_SUBROLE, is_secure_field};

/// The per-reference messaging timeout every AX read carries.
pub(crate) const MESSAGING_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_OBSERVE: Duration = Duration::from_secs(3);
/// The floor of a read timeout cut down by a deadline.
const MIN_READ: Duration = Duration::from_millis(50);
const FINGERPRINT_VALUE_CHARS: usize = 200;
/// The private attribute carrying a window's `CGWindowID`.
const WINDOW_ID_ATTRIBUTE: &str = "_AXWindowID";

/// One attribute value as the accessibility API hands it back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AxValue<N> {
    /// A successful read of no value.
    Null,
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    /// An `AXValueRef` wrapping a point or a size (`AXPosition`, `AXSize`),
    /// with its CF description.
    Geometry {
        pair: Pair,
        description: String,
    },
    Element(N),
    Elements(Vec<N>),
    /// Any other CF value, by its CF description.
    Other(String),
}

impl<N> AxValue<N> {
    /// The value as text (Python's `str(value)` over the pyobjc bridge):
    /// numbers in Python's spelling, other CF values by their description.
    /// An element-valued attribute reads as no text.
    pub(crate) fn text(&self) -> Option<String> {
        match self {
            AxValue::Text(text) => Some(text.clone()),
            AxValue::Integer(value) => Some(value.to_string()),
            AxValue::Float(value) => Some(repr_float(*value)),
            AxValue::Bool(value) => Some(if *value { "True" } else { "False" }.to_string()),
            AxValue::Geometry { description, .. } | AxValue::Other(description) => {
                Some(description.clone())
            }
            AxValue::Null | AxValue::Element(_) | AxValue::Elements(_) => None,
        }
    }

    /// The (x, y) of a position or the (width, height) of a size.
    fn pair(&self) -> Option<Pair> {
        match self {
            AxValue::Geometry { pair, .. } => Some(*pair),
            _ => None,
        }
    }

    /// Python's `int(value)`, `None` where it would raise.
    fn integer(&self) -> Option<i64> {
        match self {
            AxValue::Integer(value) => Some(*value),
            #[allow(clippy::cast_possible_truncation)] // int() truncates; the range is checked
            AxValue::Float(value) if value.is_finite() && value.abs() < 9.2e18 => {
                Some(value.trunc() as i64)
            }
            AxValue::Bool(value) => Some(i64::from(*value)),
            AxValue::Text(text) => text.trim().parse().ok(),
            _ => None,
        }
    }
}

/// A failed AX call: the `AXError` code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AxError(pub i32);

/// The raw accessibility transport (`AXUIElement*` calls).
pub(crate) trait Ax: Send + Sync {
    /// One element reference.
    type Node: Clone + Send + Sync + 'static;

    /// The application element of `pid`; `None` for a pid the API cannot address.
    fn application(&self, pid: i64) -> Option<Self::Node>;
    /// Bound every call through `node` (the timeout is per reference).
    fn set_timeout(&self, node: &Self::Node, timeout: Duration);
    fn copy(
        &self,
        node: &Self::Node,
        attribute: &str,
    ) -> std::result::Result<AxValue<Self::Node>, AxError>;
    fn action_names(&self, node: &Self::Node) -> std::result::Result<Vec<String>, AxError>;
    fn perform(&self, node: &Self::Node, action: &str) -> std::result::Result<(), AxError>;
    fn is_settable(&self, node: &Self::Node, attribute: &str)
    -> std::result::Result<bool, AxError>;
    fn set_string(
        &self,
        node: &Self::Node,
        attribute: &str,
        value: &str,
    ) -> std::result::Result<(), AxError>;
    /// Write a `CFRange` attribute.
    fn set_range(
        &self,
        node: &Self::Node,
        attribute: &str,
        location: usize,
        length: usize,
    ) -> std::result::Result<(), AxError>;
    /// The very same reference (Python's `is`).
    fn identical(&self, left: &Self::Node, right: &Self::Node) -> bool;
    /// The same element (`CFEqual`).
    fn equal(&self, left: &Self::Node, right: &Self::Node) -> bool;
}

/// The time left before `deadline`, capped at the messaging timeout and
/// floored at [`MIN_READ`].
fn remaining(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .clamp(MIN_READ, MESSAGING_TIMEOUT)
}

/// The accessibility rules over one [`Ax`] transport.
pub(crate) struct Accessibility<X: Ax> {
    pub(crate) ax: X,
}

impl<X: Ax> Accessibility<X> {
    /// Copy one attribute under the per-reference timeout; `None` on any
    /// AX error and for a null value.
    fn copy_value(
        &self,
        node: &X::Node,
        attribute: &str,
        timeout: Option<Duration>,
    ) -> Option<AxValue<X::Node>> {
        self.ax
            .set_timeout(node, timeout.unwrap_or(MESSAGING_TIMEOUT));
        match self.ax.copy(node, attribute) {
            Ok(AxValue::Null) | Err(_) => None,
            Ok(value) => Some(value),
        }
    }

    fn copy_text(
        &self,
        node: &X::Node,
        attribute: &str,
        timeout: Option<Duration>,
    ) -> Option<String> {
        self.copy_value(node, attribute, timeout)
            .and_then(|value| value.text())
    }

    /// One attribute as text, telling a failed read (`ok == false`) from a
    /// read of no value: security-relevant reads fail closed on the former.
    fn read_attribute(
        &self,
        node: &X::Node,
        attribute: &str,
        timeout: Option<Duration>,
    ) -> (bool, Option<String>) {
        self.ax
            .set_timeout(node, timeout.unwrap_or(MESSAGING_TIMEOUT));
        match self.ax.copy(node, attribute) {
            Ok(value) => (true, value.text()),
            Err(_) => (false, None),
        }
    }

    /// Snapshot the focused window of `pid`: its descendants depth-first
    /// (capped at [`MAX_DEPTH`] levels and [`MAX_ELEMENTS`] elements), the
    /// walk bounded by 3 s with every read's timeout cut to the time left.
    /// `server_rect` reads a window's bounds from the window server by its
    /// `CGWindowID` (Electron windows often omit `AXPosition`/`AXSize`).
    pub(crate) fn observe(
        &self,
        pid: i64,
        server_rect: impl Fn(i64) -> Option<Rect>,
    ) -> Observation<X::Node> {
        let Some(app) = self.ax.application(pid) else {
            return Observation::empty();
        };
        self.ax.set_timeout(&app, MESSAGING_TIMEOUT);
        let Some(AxValue::Element(window)) = self.copy_value(&app, "AXFocusedWindow", None) else {
            return Observation::empty();
        };
        // A windowless app reports the application element as its focused
        // window: there is no window tree to observe yet.
        if self.copy_text(&window, "AXRole", None).as_deref() == Some("AXApplication") {
            return Observation::empty();
        }
        let deadline = Instant::now() + MAX_OBSERVE;
        let mut walk = Walk {
            refs: Vec::new(),
            ancestors: Vec::new(),
            deadline,
            stopped: false,
        };
        let mut tree = Vec::new();
        self.walk(&window, 1, &mut tree, &mut walk);
        let timeout = Some(remaining(deadline));
        let window_id = self
            .copy_value(&window, WINDOW_ID_ATTRIBUTE, timeout)
            .and_then(|value| value.integer());
        let window_title = cap(self.copy_text(&window, "AXTitle", timeout));
        let window_rect = self
            .window_rect(&window, timeout)
            .or_else(|| window_id.and_then(&server_rect));
        let focused_index = self.focused_index(&app, &walk.refs, timeout);
        Observation {
            window_title,
            tree,
            refs: walk.refs,
            window_rect,
            focused_index,
            window_id,
            truncated: walk.stopped || Instant::now() > deadline,
        }
    }

    /// Append `parent`'s described children into `siblings`, recursing
    /// depth-first. A child identical to an ancestor is pruned (a
    /// windowless app can report itself as its own child); the deadline,
    /// the depth cap and the element cap each stop the walk as a truncation.
    fn walk(
        &self,
        parent: &X::Node,
        depth: usize,
        siblings: &mut Vec<Element>,
        walk: &mut Walk<X::Node>,
    ) {
        if Instant::now() > walk.deadline {
            walk.stopped = true;
            return;
        }
        let children = match self.copy_value(parent, "AXChildren", Some(remaining(walk.deadline))) {
            Some(AxValue::Elements(children)) => children,
            _ => Vec::new(),
        };
        for child in children {
            if walk.refs.len() >= MAX_ELEMENTS || Instant::now() > walk.deadline {
                walk.stopped = true;
                return;
            }
            if depth > MAX_DEPTH {
                walk.stopped = true;
                return;
            }
            if self.ax.identical(&child, parent)
                || walk
                    .ancestors
                    .iter()
                    .any(|ancestor| self.ax.identical(&child, ancestor))
            {
                continue;
            }
            let mut described = self.describe(&child, Some(remaining(walk.deadline)));
            walk.refs.push(child.clone());
            walk.ancestors.push(child.clone());
            let mut children = Vec::new();
            self.walk(&child, depth + 1, &mut children, walk);
            walk.ancestors.pop();
            described.children = children;
            siblings.push(described);
        }
    }

    /// One element's contract attributes, each string capped. A text field
    /// whose subrole cannot be read is treated as secure and its value is
    /// never read: an unreadable secure state fails closed.
    pub(crate) fn describe(&self, node: &X::Node, timeout: Option<Duration>) -> Element {
        let role = cap(self.copy_text(node, "AXRole", timeout));
        let (subrole_ok, subrole) = self.read_attribute(node, "AXSubrole", timeout);
        let mut subrole = cap(subrole);
        if !subrole_ok && role.as_deref() == Some(MAC_SECURE_ROLE) {
            subrole = Some(MAC_SECURE_SUBROLE.to_string());
        }
        let value = if subrole.as_deref() == Some(MAC_SECURE_SUBROLE) {
            None
        } else {
            cap(self.copy_text(node, "AXValue", timeout))
        };
        Element {
            title: cap(self.copy_text(node, "AXTitle", timeout)),
            value,
            description: cap(self.copy_text(node, "AXDescription", timeout)),
            placeholder: cap(self.copy_text(node, "AXPlaceholderValue", timeout)),
            actions: self.actions(node, timeout),
            position: self
                .copy_value(node, "AXPosition", timeout)
                .and_then(|value| value.pair()),
            size: self
                .copy_value(node, "AXSize", timeout)
                .and_then(|value| value.pair()),
            children: Vec::new(),
            role,
            subrole,
        }
    }

    /// The element's first [`MAX_ACTIONS`] action names, capped; none on an AX error.
    pub(crate) fn actions(&self, node: &X::Node, timeout: Option<Duration>) -> Vec<String> {
        self.ax
            .set_timeout(node, timeout.unwrap_or(MESSAGING_TIMEOUT));
        self.ax
            .action_names(node)
            .map(|names| {
                names
                    .into_iter()
                    .take(MAX_ACTIONS)
                    .filter_map(|name| cap(Some(name)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn window_rect(&self, window: &X::Node, timeout: Option<Duration>) -> Option<Rect> {
        let (x, y) = self.copy_value(window, "AXPosition", timeout)?.pair()?;
        let (width, height) = self.copy_value(window, "AXSize", timeout)?.pair()?;
        Some(Rect::new(x, y, width, height))
    }

    fn focused_index(
        &self,
        app: &X::Node,
        refs: &[X::Node],
        timeout: Option<Duration>,
    ) -> Option<usize> {
        let Some(AxValue::Element(focused)) = self.copy_value(app, "AXFocusedUIElement", timeout)
        else {
            return None;
        };
        refs.iter()
            .position(|node| self.ax.identical(node, &focused) || self.ax.equal(node, &focused))
    }

    /// The live role and subrole of `node` as a secure verdict; `None` when
    /// either read fails.
    fn secure_by_role(&self, node: &X::Node, timeout: Option<Duration>) -> Option<bool> {
        let (role_ok, role) = self.read_attribute(node, "AXRole", timeout);
        let (subrole_ok, subrole) = self.read_attribute(node, "AXSubrole", timeout);
        (role_ok && subrole_ok).then(|| is_secure_field(role.as_deref(), subrole.as_deref()))
    }

    /// Whether the app's live focused element is a secure field: `None`
    /// when the focus read fails (callers fail closed), `false` when the
    /// read succeeds with nothing focused.
    pub(crate) fn focused_is_secure(&self, pid: i64) -> Option<bool> {
        let app = self.ax.application(pid)?;
        self.ax.set_timeout(&app, MESSAGING_TIMEOUT);
        match self.ax.copy(&app, "AXFocusedUIElement") {
            Ok(AxValue::Null) => Some(false),
            Ok(AxValue::Element(focused)) => self.secure_by_role(&focused, None),
            // A failed read, or a focus that is not an element: unverifiable.
            Ok(_) | Err(_) => None,
        }
    }

    /// Whether one live element is a secure field now (an element that
    /// turned into a password field after the snapshot is still refused);
    /// `None` when its state cannot be read.
    pub(crate) fn live_is_secure(&self, node: &X::Node) -> Option<bool> {
        self.secure_by_role(node, None)
    }

    /// A cheap live identity of the focused window: its title and child
    /// count, then the focused element's role, subrole and (never for a
    /// secure or unverifiable field) value head. `None` when the focused
    /// window cannot be read.
    pub(crate) fn window_fingerprint(
        &self,
        pid: i64,
        timeout: Option<Duration>,
    ) -> Option<Fingerprint> {
        let timeout = timeout.map_or(MESSAGING_TIMEOUT, |timeout| timeout.max(MIN_READ));
        let app = self.ax.application(pid)?;
        self.ax.set_timeout(&app, timeout);
        let window = self.copy_value(&app, "AXFocusedWindow", Some(timeout))?;
        let title = match &window {
            AxValue::Element(window) => self.copy_text(window, "AXTitle", Some(timeout)),
            _ => None,
        };
        let count = match &window {
            AxValue::Element(window) => {
                match self.copy_value(window, "AXChildren", Some(timeout)) {
                    Some(AxValue::Elements(children)) => children.len(),
                    _ => 0,
                }
            }
            _ => 0,
        };
        let Some(AxValue::Element(focused)) =
            self.copy_value(&app, "AXFocusedUIElement", Some(timeout))
        else {
            return Some(vec![title, Some(count.to_string()), None, None, None]);
        };
        let (role_ok, role) = self.read_attribute(&focused, "AXRole", Some(timeout));
        let (subrole_ok, subrole) = self.read_attribute(&focused, "AXSubrole", Some(timeout));
        let secure = role.as_deref() == Some(MAC_SECURE_ROLE)
            && subrole.as_deref() == Some(MAC_SECURE_SUBROLE);
        let value_head = if !role_ok || !subrole_ok || secure {
            Some(String::new())
        } else {
            cap(self.copy_text(&focused, "AXValue", Some(timeout)))
                .map(|value| value.chars().take(FINGERPRINT_VALUE_CHARS).collect())
        };
        Some(vec![
            title,
            Some(count.to_string()),
            role,
            subrole,
            value_head,
        ])
    }

    /// One live element's current (role, title).
    pub(crate) fn live_fingerprint(&self, node: &X::Node) -> (Option<String>, Option<String>) {
        (
            self.copy_text(node, "AXRole", None),
            self.copy_text(node, "AXTitle", None),
        )
    }

    pub(crate) fn current_value(&self, node: &X::Node) -> Option<String> {
        self.copy_text(node, "AXValue", None)
    }

    /// # Errors
    ///
    /// `ACTION_UNSUPPORTED` naming the AX error.
    pub(crate) fn perform(&self, node: &X::Node, action: &str) -> Result<()> {
        self.ax.set_timeout(node, MESSAGING_TIMEOUT);
        self.ax.perform(node, action).map_err(|AxError(code)| {
            unsupported(format!(
                "the element did not perform {action} (AX error {code})"
            ))
            .with_details(json!({"action": head(action, 32)}))
        })
    }

    pub(crate) fn is_settable(&self, node: &X::Node, attribute: &str) -> bool {
        self.ax.set_timeout(node, MESSAGING_TIMEOUT);
        self.ax.is_settable(node, attribute).unwrap_or(false)
    }

    /// # Errors
    ///
    /// `ACTION_UNSUPPORTED` naming the AX error.
    pub(crate) fn set_value(&self, node: &X::Node, value: &str) -> Result<()> {
        self.ax.set_timeout(node, MESSAGING_TIMEOUT);
        self.ax
            .set_string(node, "AXValue", value)
            .map_err(|AxError(code)| {
                unsupported(format!("setting the value failed with AX error {code}"))
                    .with_details(json!({}))
            })
    }

    /// Set the selected text range, leaving the content untouched.
    ///
    /// # Errors
    ///
    /// `ACTION_UNSUPPORTED` naming the AX error.
    pub(crate) fn select_range(
        &self,
        node: &X::Node,
        location: usize,
        length: usize,
    ) -> Result<()> {
        self.ax.set_timeout(node, MESSAGING_TIMEOUT);
        self.ax
            .set_range(node, "AXSelectedTextRange", location, length)
            .map_err(|AxError(code)| -> ComputerUseError {
                unsupported(format!(
                    "selecting the text range failed with AX error {code}"
                ))
                .with_details(json!({"location": location, "length": length}))
            })
    }
}

#[cfg(test)]
impl<X: Ax> Accessibility<X> {
    /// Walk below `root` against `deadline`: the tree, the refs, and
    /// whether a bound stopped the walk.
    pub(crate) fn walk_until(
        &self,
        root: &X::Node,
        deadline: Instant,
    ) -> (Vec<Element>, Vec<X::Node>, bool) {
        let mut walk = Walk {
            refs: Vec::new(),
            ancestors: Vec::new(),
            deadline,
            stopped: false,
        };
        let mut tree = Vec::new();
        self.walk(root, 1, &mut tree, &mut walk);
        (tree, walk.refs, walk.stopped)
    }
}

/// The walk's shared state.
struct Walk<N> {
    refs: Vec<N>,
    /// The chain from the window down to the current parent.
    ancestors: Vec<N>,
    deadline: Instant,
    stopped: bool,
}
