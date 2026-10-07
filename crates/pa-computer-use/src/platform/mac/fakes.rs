//! Test doubles below the macOS seams (the skill's `FakeAXApp` family and
//! the `fakes.py` frameworks): a scripted AX tree and a recording desktop.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::ax::{Ax, AxError, AxValue};
use super::events::MacEvent;
use super::{Desktop, PostError, WorkspaceApp};
use crate::element::{Pair, Rect};
use crate::platform::{Clipboard, ClipboardSnapshot, PasteFormat, RecognizedText, TextRecognizer};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// kAXErrorCannotComplete, kAXErrorAttributeUnsupported.
pub(crate) const CANNOT_COMPLETE: AxError = AxError(-25204);
pub(crate) const UNSUPPORTED: AxError = AxError(-25205);

type Answer = Result<AxValue<Node>, AxError>;

/// One fake element's scripted attributes.
pub(crate) struct NodeData {
    /// Two references with one `element` id are `CFEqual` but not identical.
    pub element: u32,
    attributes: Mutex<BTreeMap<String, Answer>>,
    actions: Mutex<Result<Vec<String>, AxError>>,
    settable: Mutex<Result<bool, AxError>>,
    /// The answer for every attribute without its own (unset: unsupported).
    fallback: Mutex<Option<Answer>>,
}

/// One fake element reference; equality is identity.
#[derive(Clone)]
pub(crate) struct Node(pub Arc<NodeData>);

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Node").field(&self.0.element).finish()
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Node {
    pub(crate) fn new(element: u32) -> Self {
        Self(Arc::new(NodeData {
            element,
            attributes: Mutex::new(BTreeMap::new()),
            actions: Mutex::new(Ok(Vec::new())),
            settable: Mutex::new(Ok(false)),
            fallback: Mutex::new(None),
        }))
    }

    /// A second reference to the same element (`CFEqual`, not identical).
    pub(crate) fn alias(&self) -> Self {
        let alias = Node::new(self.0.element);
        *lock(&alias.0.attributes) = lock(&self.0.attributes).clone();
        alias
    }

    pub(crate) fn set(&self, attribute: &str, answer: Answer) -> &Self {
        lock(&self.0.attributes).insert(attribute.to_string(), answer);
        self
    }

    pub(crate) fn text(self, attribute: &str, value: &str) -> Self {
        self.set(attribute, Ok(AxValue::Text(value.to_string())));
        self
    }

    pub(crate) fn fails(self, attribute: &str) -> Self {
        self.set(attribute, Err(CANNOT_COMPLETE));
        self
    }

    pub(crate) fn null(self, attribute: &str) -> Self {
        self.set(attribute, Ok(AxValue::Null));
        self
    }

    pub(crate) fn geometry(self, position: Pair, size: Pair) -> Self {
        self.set("AXPosition", Ok(geometry(position)));
        self.set("AXSize", Ok(geometry(size)));
        self
    }

    pub(crate) fn children(self, children: Vec<Node>) -> Self {
        self.set("AXChildren", Ok(AxValue::Elements(children)));
        self
    }

    pub(crate) fn actions(self, actions: Result<Vec<String>, AxError>) -> Self {
        *lock(&self.0.actions) = actions;
        self
    }

    pub(crate) fn settable(self, settable: Result<bool, AxError>) -> Self {
        *lock(&self.0.settable) = settable;
        self
    }

    /// Every unscripted attribute answers `answer`.
    pub(crate) fn otherwise(self, answer: Answer) -> Self {
        *lock(&self.0.fallback) = Some(answer);
        self
    }
}

pub(crate) fn geometry(pair: Pair) -> AxValue<Node> {
    AxValue::Geometry {
        pair,
        description: format!("<AXValue {pair:?}>"),
    }
}

/// One recorded AX call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AxCall {
    Timeout(u32, Duration),
    Read(u32, String),
    Actions(u32),
    Perform(u32, String),
    Settable(u32, String),
    SetString(u32, String, String),
    SetRange(u32, String, usize, usize),
}

#[derive(Default)]
pub(crate) struct AxState {
    pub apps: BTreeMap<i64, Node>,
    pub calls: Vec<AxCall>,
    pub perform_error: Option<AxError>,
    pub write_error: Option<AxError>,
}

/// The scripted AX transport; clones share state.
#[derive(Clone, Default)]
pub(crate) struct FakeAx(pub Arc<Mutex<AxState>>);

impl FakeAx {
    pub(crate) fn with_app(pid: i64, app: Node) -> Self {
        let fake = Self::default();
        fake.state().apps.insert(pid, app);
        fake
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, AxState> {
        lock(&self.0)
    }

    pub(crate) fn calls(&self) -> Vec<AxCall> {
        self.state().calls.clone()
    }

    /// The attributes read, in order.
    pub(crate) fn reads(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                AxCall::Read(_, attribute) => Some(attribute),
                _ => None,
            })
            .collect()
    }

    /// The elements a timeout was set on.
    pub(crate) fn timed_out_elements(&self) -> Vec<u32> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                AxCall::Timeout(element, _) => Some(element),
                _ => None,
            })
            .collect()
    }

    fn record(&self, call: AxCall) {
        self.state().calls.push(call);
    }
}

impl Ax for FakeAx {
    type Node = Node;

    fn application(&self, pid: i64) -> Option<Node> {
        self.state().apps.get(&pid).cloned()
    }

    fn set_timeout(&self, node: &Node, timeout: Duration) {
        self.record(AxCall::Timeout(node.0.element, timeout));
    }

    fn copy(&self, node: &Node, attribute: &str) -> Answer {
        self.record(AxCall::Read(node.0.element, attribute.to_string()));
        let scripted = lock(&node.0.attributes).get(attribute).cloned();
        scripted
            .or_else(|| lock(&node.0.fallback).clone())
            .unwrap_or(Err(UNSUPPORTED))
    }

    fn action_names(&self, node: &Node) -> Result<Vec<String>, AxError> {
        self.record(AxCall::Actions(node.0.element));
        lock(&node.0.actions).clone()
    }

    fn perform(&self, node: &Node, action: &str) -> Result<(), AxError> {
        self.record(AxCall::Perform(node.0.element, action.to_string()));
        self.state().perform_error.map_or(Ok(()), Err)
    }

    fn is_settable(&self, node: &Node, attribute: &str) -> Result<bool, AxError> {
        self.record(AxCall::Settable(node.0.element, attribute.to_string()));
        *lock(&node.0.settable)
    }

    fn set_string(&self, node: &Node, attribute: &str, value: &str) -> Result<(), AxError> {
        self.record(AxCall::SetString(
            node.0.element,
            attribute.to_string(),
            value.to_string(),
        ));
        self.state().write_error.map_or(Ok(()), Err)
    }

    fn set_range(
        &self,
        node: &Node,
        attribute: &str,
        location: usize,
        length: usize,
    ) -> Result<(), AxError> {
        self.record(AxCall::SetRange(
            node.0.element,
            attribute.to_string(),
            location,
            length,
        ));
        self.state().write_error.map_or(Ok(()), Err)
    }

    fn identical(&self, left: &Node, right: &Node) -> bool {
        left == right
    }

    fn equal(&self, left: &Node, right: &Node) -> bool {
        left.0.element == right.0.element
    }
}

#[derive(Default)]
pub(crate) struct DesktopState {
    pub running: Vec<WorkspaceApp>,
    /// An app that shows up once `running_applications` was read this many times.
    pub launched: Option<(WorkspaceApp, usize)>,
    pub running_reads: usize,
    pub frontmost: Option<i64>,
    pub activated: Vec<i64>,
    pub bundles: BTreeMap<PathBuf, String>,
    pub locked: Option<bool>,
    pub trusted: Option<bool>,
    pub screen_capture: Option<bool>,
    pub windows: Option<Vec<(i64, Rect)>>,
    pub window_reads: usize,
    pub posted: Vec<(i64, Vec<MacEvent>)>,
    pub post_error: Option<PostError>,
}

/// The recording desktop; clones share state.
#[derive(Clone, Default)]
pub(crate) struct FakeDesktop(pub Arc<Mutex<DesktopState>>);

impl FakeDesktop {
    pub(crate) fn state(&self) -> MutexGuard<'_, DesktopState> {
        lock(&self.0)
    }
}

pub(crate) fn workspace_app(bundle_id: &str, name: &str, pid: i64) -> WorkspaceApp {
    WorkspaceApp {
        bundle_id: Some(bundle_id.to_string()),
        name: Some(name.to_string()),
        pid,
        path: Some(format!("/Applications/{name}.app")),
        regular: true,
    }
}

impl Desktop for FakeDesktop {
    fn running_applications(&self) -> Vec<WorkspaceApp> {
        let mut state = self.state();
        state.running_reads += 1;
        if let Some((app, after)) = state.launched.clone() {
            if state.running_reads > after {
                state.running.push(app);
                state.launched = None;
            }
        }
        state.running.clone()
    }

    fn frontmost_pid(&self) -> Option<i64> {
        self.state().frontmost
    }

    fn activate(&self, pid: i64) -> bool {
        let mut state = self.state();
        if !state.running.iter().any(|app| app.pid == pid) {
            return false;
        }
        state.activated.push(pid);
        true
    }

    fn bundle_identifier(&self, bundle_dir: &Path) -> Option<String> {
        self.state().bundles.get(bundle_dir).cloned()
    }

    fn session_locked(&self) -> Option<bool> {
        self.state().locked
    }

    fn accessibility_trusted(&self) -> Option<bool> {
        self.state().trusted
    }

    fn screen_capture_allowed(&self) -> Option<bool> {
        self.state().screen_capture
    }

    fn on_screen_windows(&self) -> Option<Vec<(i64, Rect)>> {
        let mut state = self.state();
        state.window_reads += 1;
        state.windows.clone()
    }

    fn post(&self, pid: i64, events: &[MacEvent]) -> Result<(), PostError> {
        let mut state = self.state();
        if let Some(error) = state.post_error.clone() {
            return Err(error);
        }
        state.posted.push((pid, events.to_vec()));
        Ok(())
    }
}

impl Clipboard for FakeDesktop {
    fn save(&self) -> Option<ClipboardSnapshot> {
        Some(Vec::new())
    }

    fn write(&self, _text: &str, _format: PasteFormat) -> Result<(), String> {
        Ok(())
    }

    fn holds(&self, _text: &str) -> bool {
        true
    }

    fn change_count(&self) -> Option<i64> {
        Some(1)
    }

    fn restore(&self, _snapshot: &ClipboardSnapshot) {}
}

impl TextRecognizer for FakeDesktop {
    fn recognize(&self, _path: &str) -> Result<Vec<RecognizedText>, String> {
        Ok(Vec::new())
    }
}
