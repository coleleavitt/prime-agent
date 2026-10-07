//! The Wayland backend's test doubles (the skill's `fakes_wayland`): a
//! scripted niri IPC server, a fake AT-SPI object graph, and a recorder at
//! the virtual-input seam. Nothing touches a real session.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{json, Value};

use super::atspi::{AtSpi, States};
use super::input::{Availability, KeyStroke, PointerTarget, VirtualInput};
use super::niri::NiriTransport;
use crate::element::Pair;
use crate::error::{ComputerUseError, Result};
use crate::platform::{MouseButton, ScrollDirection};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- niri ----------------------------------------------------------------------

/// One niri window record in the IPC shape (niri 26.04 `niri msg --json`).
#[allow(clippy::too_many_arguments)] // the fixture spells every layout field
pub(crate) fn niri_window(
    id: i64,
    app_id: &str,
    title: &str,
    pid: i64,
    workspace_id: i64,
    focused: bool,
    tile_pos: Option<(f64, f64)>,
    offset: (f64, f64),
    size: (i32, i32),
    focus_secs: i64,
) -> Value {
    json!({
        "id": id,
        "title": title,
        "app_id": app_id,
        "pid": pid,
        "workspace_id": workspace_id,
        "is_focused": focused,
        "is_floating": tile_pos.is_some(),
        "is_urgent": false,
        "layout": {
            "pos_in_scrolling_layout": if tile_pos.is_some() { Value::Null } else { json!([1, 1]) },
            "tile_size": [f64::from(size.0), f64::from(size.1)],
            "window_size": [size.0, size.1],
            "tile_pos_in_workspace_view": tile_pos.map(|(x, y)| json!([x, y])),
            "window_offset_in_tile": [offset.0, offset.1],
        },
        "focus_timestamp": {"secs": focus_secs, "nanos": 0},
    })
}

/// The default session: a floating editor (focused), its tiled second
/// window, a terminal, a floating app on the HDMI output.
pub(crate) fn default_windows() -> Vec<Value> {
    vec![
        niri_window(
            10,
            "org.gnome.TextEditor",
            "Doc - Text Editor",
            501,
            1,
            true,
            Some((100.0, 50.0)),
            (4.0, 6.0),
            (800, 600),
            300,
        ),
        niri_window(
            11,
            "org.gnome.TextEditor",
            "Other doc",
            501,
            1,
            false,
            None,
            (0.0, 0.0),
            (800, 600),
            200,
        ),
        niri_window(
            20,
            "foot",
            "shell",
            600,
            1,
            false,
            None,
            (0.0, 0.0),
            (800, 600),
            100,
        ),
        niri_window(
            30,
            "org.example.Floaty",
            "Floaty",
            700,
            2,
            false,
            Some((10.0, 20.0)),
            (2.0, 3.0),
            (400, 300),
            50,
        ),
    ]
}

pub(crate) struct NiriState {
    pub windows: Vec<Value>,
    pub workspaces: Value,
    pub outputs: Value,
    pub requests: Vec<Value>,
    /// When false, `FocusWindow` leaves the focus where it was.
    pub focus_lands: bool,
    pub fail: Option<String>,
    /// Runs after a `FocusWindow` lands (a focus change moving onto a field).
    pub on_focus: Option<Arc<dyn Fn(i64) + Send + Sync>>,
}

/// The scripted niri server; clones share state.
#[derive(Clone)]
pub(crate) struct FakeNiri {
    pub state: Arc<Mutex<NiriState>>,
}

impl Default for FakeNiri {
    fn default() -> Self {
        Self::with_windows(default_windows())
    }
}

impl FakeNiri {
    pub(crate) fn with_windows(windows: Vec<Value>) -> Self {
        Self {
            state: Arc::new(Mutex::new(NiriState {
                windows,
                workspaces: json!([
                    {"id": 1, "idx": 1, "output": "eDP-1", "is_active": true, "is_focused": true},
                    {"id": 2, "idx": 1, "output": "HDMI-A-1", "is_active": true, "is_focused": false},
                    {"id": 3, "idx": 2, "output": "eDP-1", "is_active": false, "is_focused": false},
                ]),
                outputs: json!({
                    "eDP-1": {"name": "eDP-1", "logical": {"x": 0, "y": 0, "width": 1920, "height": 1200, "scale": 2.0}},
                    "HDMI-A-1": {"name": "HDMI-A-1", "logical": {"x": 1920, "y": 0, "width": 2560, "height": 1440, "scale": 1.0}},
                }),
                requests: Vec::new(),
                focus_lands: true,
                fail: None,
                on_focus: None,
            })),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, NiriState> {
        lock(&self.state)
    }

    pub(crate) fn focus(&self, window_id: i64) {
        for window in &mut self.state().windows {
            window["is_focused"] = json!(window["id"] == json!(window_id));
        }
    }

    pub(crate) fn actions(&self) -> Vec<Value> {
        self.state()
            .requests
            .iter()
            .filter(|request| request.get("Action").is_some())
            .cloned()
            .collect()
    }
}

impl NiriTransport for FakeNiri {
    fn exchange(&self, line: &[u8]) -> Result<Vec<u8>> {
        assert!(line.ends_with(b"\n"), "one request line");
        let request: Value = serde_json::from_slice(line).expect("a JSON request");
        let (ok, focus) = {
            let mut state = self.state();
            state.requests.push(request.clone());
            if let Some(fail) = &state.fail {
                return Ok(format!("{}\n", json!({"Err": fail})).into_bytes());
            }
            match request.as_str() {
                Some("Windows") => (json!({"Windows": state.windows}), None),
                Some("Workspaces") => (json!({"Workspaces": state.workspaces}), None),
                Some("Outputs") => (json!({"Outputs": state.outputs}), None),
                Some("FocusedWindow") => {
                    let focused = state
                        .windows
                        .iter()
                        .find(|window| window["is_focused"] == json!(true))
                        .cloned()
                        .unwrap_or(Value::Null);
                    (json!({"FocusedWindow": focused}), None)
                }
                _ => match request
                    .pointer("/Action/FocusWindow/id")
                    .and_then(Value::as_i64)
                {
                    Some(target) => (json!("Handled"), state.focus_lands.then_some(target)),
                    None => return Ok(b"{\"Err\":\"error parsing request\"}\n".to_vec()),
                },
            }
        };
        if let Some(target) = focus {
            self.focus(target);
            let hook = self.state().on_focus.clone();
            if let Some(hook) = hook {
                hook(target);
            }
        }
        Ok(format!("{}\n", json!({"Ok": ok})).into_bytes())
    }
}

// --- AT-SPI --------------------------------------------------------------------

/// One fake accessible's mutable state.
#[derive(Default)]
#[allow(clippy::struct_excessive_bools)] // one flag per scripted AT-SPI behaviour
pub(crate) struct Accessible {
    pub role: &'static str,
    pub name: Option<String>,
    pub children: Vec<Node>,
    pub states: BTreeSet<&'static str>,
    pub actions: Vec<String>,
    pub text: Option<String>,
    pub editable: bool,
    pub extents: Option<(i32, i32, i32, i32)>,
    pub description: Option<String>,
    pub pid: Option<i64>,
    pub value: Option<f64>,
    /// Every call fails like a defunct object.
    pub broken: bool,
    /// The role read fails.
    pub role_fails: bool,
    pub reads: Vec<&'static str>,
    pub performed: Vec<String>,
    pub writes: Vec<String>,
    pub selections: Vec<(i64, i64)>,
    pub do_action_result: bool,
    /// Component.GrabFocus: `None` not exposed, `Some(true)` focuses,
    /// `Some(false)` errors like GTK 4.
    pub grab_focus: Option<bool>,
}

/// One fake accessible; equality is identity.
#[derive(Clone)]
pub(crate) struct Node(pub Arc<Mutex<Accessible>>);

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let accessible = self.get();
        f.debug_tuple("Node")
            .field(&accessible.role)
            .field(&accessible.name)
            .finish()
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Node {
    pub(crate) fn new(role: &'static str, name: Option<&str>) -> Self {
        Self(Arc::new(Mutex::new(Accessible {
            role,
            name: name.map(ToString::to_string),
            states: BTreeSet::from(["SHOWING", "VISIBLE"]),
            do_action_result: true,
            ..Accessible::default()
        })))
    }

    pub(crate) fn get(&self) -> MutexGuard<'_, Accessible> {
        lock(&self.0)
    }

    pub(crate) fn with(self, edit: impl FnOnce(&mut Accessible)) -> Self {
        edit(&mut self.get());
        self
    }

    pub(crate) fn child(&self, index: usize) -> Node {
        self.get().children[index].clone()
    }
}

fn role_name(role: &str) -> &'static str {
    match role {
        "FRAME" => "frame",
        "PUSH_BUTTON" => "push button",
        "ENTRY" => "entry",
        "PASSWORD_TEXT" => "password text",
        "PANEL" => "panel",
        "LABEL" => "label",
        "MENU" => "menu",
        "MENU_ITEM" => "menu item",
        "APPLICATION" => "application",
        _ => "dialog",
    }
}

/// The fake bus: the desktop's applications.
#[derive(Clone)]
pub(crate) struct FakeAtSpi {
    pub apps: Vec<Node>,
    pub unavailable: Arc<Mutex<Option<String>>>,
}

impl FakeAtSpi {
    pub(crate) fn new(apps: Vec<Node>) -> Self {
        Self {
            apps,
            unavailable: Arc::default(),
        }
    }
}

impl Default for FakeAtSpi {
    fn default() -> Self {
        Self::new(vec![editor_app(), floaty_app()])
    }
}

fn read<T>(node: &Node, read: impl FnOnce(&Accessible) -> Option<T>) -> Option<T> {
    let accessible = node.get();
    if accessible.broken {
        return None;
    }
    read(&accessible)
}

impl AtSpi for FakeAtSpi {
    type Node = Node;

    fn available(&self) -> std::result::Result<(), String> {
        lock(&self.unavailable).clone().map_or(Ok(()), Err)
    }
    fn applications(&self) -> Option<Vec<Node>> {
        Some(self.apps.clone())
    }
    fn process_id(&self, node: &Node) -> Option<i64> {
        read(node, |a| a.pid)
    }
    fn child_count(&self, node: &Node) -> Option<usize> {
        read(node, |a| Some(a.children.len()))
    }
    fn child(&self, node: &Node, index: usize) -> Option<Node> {
        read(node, |a| a.children.get(index).cloned())
    }
    fn states(&self, node: &Node) -> Option<States> {
        read(node, |a| {
            Some(States {
                showing: a.states.contains("SHOWING"),
                focused: a.states.contains("FOCUSED"),
                active: a.states.contains("ACTIVE"),
                editable: a.states.contains("EDITABLE"),
            })
        })
    }
    fn is_password(&self, node: &Node) -> Option<bool> {
        read(node, |a| {
            (!a.role_fails).then_some(a.role == "PASSWORD_TEXT")
        })
    }
    fn role_name(&self, node: &Node) -> Option<String> {
        read(node, |a| Some(role_name(a.role).to_string()))
    }
    fn name(&self, node: &Node) -> Option<String> {
        read(node, |a| a.name.clone())
    }
    fn description(&self, node: &Node) -> Option<String> {
        read(node, |a| a.description.clone())
    }
    fn has_text(&self, node: &Node) -> bool {
        read(node, |a| a.text.as_ref().map(drop)).is_some()
    }
    fn character_count(&self, node: &Node) -> Option<i64> {
        read(node, |a| {
            a.text
                .as_ref()
                .map(|text| i64::try_from(text.chars().count()).unwrap())
        })
    }
    fn text(&self, node: &Node, start: i64, end: i64) -> Option<String> {
        let mut accessible = node.get();
        accessible.reads.push("text");
        let text = accessible.text.as_ref()?;
        let (start, end) = (usize::try_from(start).ok()?, usize::try_from(end).ok()?);
        Some(text.chars().skip(start).take(end - start).collect())
    }
    fn current_value(&self, node: &Node) -> Option<f64> {
        read(node, |a| a.value)
    }
    fn action_count(&self, node: &Node) -> Option<usize> {
        read(node, |a| (!a.actions.is_empty()).then_some(a.actions.len()))
    }
    fn action_name(&self, node: &Node, index: usize) -> Option<String> {
        read(node, |a| a.actions.get(index).cloned())
    }
    fn do_action(&self, node: &Node, index: usize) -> std::result::Result<bool, String> {
        let mut accessible = node.get();
        let action = accessible.actions[index].clone();
        accessible.performed.push(action);
        Ok(accessible.do_action_result)
    }
    fn extents(&self, node: &Node) -> Option<(i32, i32, i32, i32)> {
        read(node, |a| a.extents)
    }
    fn grab_focus(&self, node: &Node) -> Option<bool> {
        let mut accessible = node.get();
        if !accessible.grab_focus? {
            return None;
        }
        accessible.states.insert("FOCUSED");
        Some(true)
    }
    fn has_editable_text(&self, node: &Node) -> bool {
        read(node, |a| a.editable.then_some(())).is_some()
    }
    fn set_text_contents(&self, node: &Node, value: &str) -> std::result::Result<bool, String> {
        let mut accessible = node.get();
        accessible.writes.push(value.to_string());
        accessible.text = Some(value.to_string());
        Ok(true)
    }
    fn selection_count(&self, node: &Node) -> Option<i64> {
        read(node, |a| Some(i64::try_from(a.selections.len()).unwrap()))
    }
    fn set_selection(
        &self,
        node: &Node,
        start: i64,
        end: i64,
    ) -> std::result::Result<bool, String> {
        node.get().selections.push((start, end));
        Ok(true)
    }
    fn add_selection(
        &self,
        node: &Node,
        start: i64,
        end: i64,
    ) -> std::result::Result<bool, String> {
        node.get().selections.push((start, end));
        Ok(true)
    }
}

/// The editor (pid 501): two frames, the first matching window 10.
pub(crate) fn editor_app() -> Node {
    let save = Node::new("PUSH_BUTTON", Some("Save")).with(|a| {
        a.actions = vec!["click".to_string()];
        a.extents = Some((10, 10, 80, 30));
    });
    let search = Node::new("ENTRY", Some("Search")).with(|a| {
        a.text = Some("hello world hello".to_string());
        a.editable = true;
        a.states = BTreeSet::from(["SHOWING", "VISIBLE", "EDITABLE", "FOCUSED"]);
        a.extents = Some((100, 10, 200, 30));
    });
    let password = Node::new("PASSWORD_TEXT", Some("Password")).with(|a| {
        a.text = Some("hunter2".to_string());
        a.editable = true;
        a.states = BTreeSet::from(["SHOWING", "VISIBLE", "EDITABLE"]);
        a.extents = Some((100, 50, 200, 30));
    });
    let label = Node::new("LABEL", Some("Status")).with(|a| {
        a.text = Some("Status".to_string());
        a.extents = Some((10, 100, 50, 20));
    });
    let panel = Node::new("PANEL", None).with(|a| {
        a.children = vec![label];
        a.extents = Some((0, 90, 400, 40));
    });
    let hidden_item = Node::new("MENU_ITEM", Some("Quit")).with(|a| {
        a.actions = vec!["click".to_string()];
        a.states = BTreeSet::from(["VISIBLE"]);
    });
    let hidden_menu = Node::new("MENU", Some("File")).with(|a| {
        a.children = vec![hidden_item];
        a.states = BTreeSet::from(["VISIBLE"]);
    });
    let frame = Node::new("FRAME", Some("Doc - Text Editor")).with(|a| {
        a.children = vec![save, search, password, panel, hidden_menu];
        a.states = BTreeSet::from(["SHOWING", "VISIBLE", "ACTIVE"]);
        a.extents = Some((0, 0, 800, 600));
    });
    let other = Node::new("FRAME", Some("Other doc")).with(|a| a.extents = Some((0, 0, 800, 600)));
    Node::new("APPLICATION", Some("gnome-text-editor")).with(|a| {
        a.children = vec![frame, other];
        a.pid = Some(501);
    })
}

/// The floating app on HDMI (pid 700).
pub(crate) fn floaty_app() -> Node {
    let ok = Node::new("PUSH_BUTTON", Some("OK")).with(|a| {
        a.actions = vec!["press".to_string()];
        a.extents = Some((20, 30, 60, 20));
    });
    let frame = Node::new("FRAME", Some("Floaty")).with(|a| {
        a.children = vec![ok];
        a.extents = Some((0, 0, 400, 300));
    });
    Node::new("APPLICATION", Some("floaty")).with(|a| {
        a.children = vec![frame];
        a.pid = Some(700);
    })
}

// --- virtual input -------------------------------------------------------------

/// One recorded virtual-input call, with the focused window at the time.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Input {
    Click(PointerTarget, Pair, MouseButton, u32, Option<i64>),
    Drag(PointerTarget, Pair, Pair, Option<i64>),
    Scroll(PointerTarget, Pair, ScrollDirection, u32, Option<i64>),
    Keys(Vec<KeyStroke>, Option<i64>),
}

/// A hook run on a pointer click.
pub(crate) type ClickHook = Box<dyn Fn() + Send>;

/// Records every call at the virtual-input seam.
#[derive(Clone)]
pub(crate) struct InputRecorder {
    pub niri: FakeNiri,
    pub calls: Arc<Mutex<Vec<Input>>>,
    /// Runs on a pointer click (a real click focusing a field).
    pub on_click: Arc<Mutex<Option<ClickHook>>>,
    pub unavailable: Arc<Mutex<Option<ComputerUseError>>>,
}

impl InputRecorder {
    pub(crate) fn new(niri: FakeNiri) -> Self {
        Self {
            niri,
            calls: Arc::default(),
            on_click: Arc::default(),
            unavailable: Arc::default(),
        }
    }

    pub(crate) fn calls(&self) -> Vec<Input> {
        lock(&self.calls).clone()
    }

    fn focused(&self) -> Option<i64> {
        let state = self.niri.state();
        state
            .windows
            .iter()
            .find(|window| window["is_focused"] == json!(true))
            .and_then(|window| window["id"].as_i64())
    }
}

impl VirtualInput for InputRecorder {
    fn available(&self) -> Result<Availability> {
        match lock(&self.unavailable).clone() {
            Some(error) => Err(error),
            None => Ok(Availability {
                pointer: true,
                keyboard: true,
            }),
        }
    }
    fn click(
        &self,
        target: &PointerTarget,
        point: Pair,
        button: MouseButton,
        count: u32,
    ) -> Result<()> {
        let focused = self.focused();
        lock(&self.calls).push(Input::Click(target.clone(), point, button, count, focused));
        if let Some(hook) = lock(&self.on_click).as_ref() {
            hook();
        }
        Ok(())
    }
    fn drag(&self, target: &PointerTarget, start: Pair, end: Pair) -> Result<()> {
        let focused = self.focused();
        lock(&self.calls).push(Input::Drag(target.clone(), start, end, focused));
        Ok(())
    }
    fn scroll(
        &self,
        target: &PointerTarget,
        point: Pair,
        direction: ScrollDirection,
        clicks: u32,
    ) -> Result<()> {
        let focused = self.focused();
        lock(&self.calls).push(Input::Scroll(
            target.clone(),
            point,
            direction,
            clicks,
            focused,
        ));
        Ok(())
    }
    fn send_keys(&self, strokes: &[KeyStroke]) -> Result<()> {
        let focused = self.focused();
        lock(&self.calls).push(Input::Keys(strokes.to_vec(), focused));
        Ok(())
    }
}
