//! Ported from the skill's `tests/test_wayland.py`: niri IPC, observation,
//! secure focus, fingerprints, AT-SPI element operations, input dispatch,
//! screenshots, lock and status, and the App layer end to end on the fakes.

use std::time::Duration;

use serde_json::json;

use super::fakes::{
    editor_app, floaty_app, niri_window, FakeAtSpi, FakeNiri, Input, InputRecorder, Node,
};
use super::input::{KeyStroke, PointerTarget};
use super::*;
use crate::element::{Element, MAX_DEPTH, MAX_ELEMENTS};
use crate::error::ErrorCode;
use crate::keymap::parse_chord;
use crate::process::script::Script;
use crate::render::serialize;
use crate::session::fake::Env;
use crate::session::{ActionArg, AppCall, IndexArg, PointArg, TargetArg, TextArg};
use crate::telemetry::Outcome;

type Fake = WaylandPlatform<Script, FakeNiri, FakeAtSpi, InputRecorder>;

struct World {
    platform: Fake,
    niri: FakeNiri,
    script: Script,
    input: InputRecorder,
    shots: tempfile::TempDir,
}

fn world_with(niri: FakeNiri, atspi: FakeAtSpi, tools: &[&str]) -> World {
    let script = Script::with_tools(tools);
    script.on(&["show-session"], 0, b"LockedHint=no\nActive=yes\n", b"");
    let input = InputRecorder::new(niri.clone());
    let shots = tempfile::tempdir().unwrap();
    let capture = CaptureDir::new(
        shots.path().join("shots"),
        shots.path().join("unrelated-home"),
    );
    World {
        platform: WaylandPlatform::new(
            script.clone(),
            niri.clone(),
            atspi,
            input.clone(),
            capture,
            Duration::from_millis(50),
        ),
        niri,
        script,
        input,
        shots,
    }
}

fn world() -> World {
    world_with(
        FakeNiri::default(),
        FakeAtSpi::default(),
        &["grim", "loginctl"],
    )
}

// --- niri IPC ------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn the_socket_transport_speaks_json_lines() {
    use std::io::{BufRead, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("niri.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut line)
            .unwrap();
        (&stream)
            .write_all(b"{\"Ok\":{\"FocusedWindow\":null}}\n")
            .unwrap();
        line
    });
    let reply = super::niri::exchange_at(path.to_str().unwrap(), b"\"FocusedWindow\"\n").unwrap();
    assert_eq!(reply, b"{\"Ok\":{\"FocusedWindow\":null}}\n");
    assert_eq!(server.join().unwrap(), "\"FocusedWindow\"\n");
    let missing = super::niri::exchange_at(dir.path().join("gone.sock").to_str().unwrap(), b"x\n")
        .unwrap_err();
    assert!(
        missing.message.starts_with("niri IPC failed: "),
        "{}",
        missing.message
    );
}

struct Canned(&'static [u8]);

impl NiriTransport for Canned {
    fn exchange(&self, _line: &[u8]) -> Result<Vec<u8>> {
        Ok(self.0.to_vec())
    }
}

#[test]
fn err_and_garbage_replies_are_transport_errors() {
    for reply in [
        &b"{\"Err\":\"nope\"}\n"[..],
        b"not json\n",
        b"{\"Weird\":1}\n",
    ] {
        let error = Niri {
            transport: Canned(reply),
        }
        .windows()
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::TransportError, "{reply:?}");
    }
    assert_eq!(
        Niri {
            transport: Canned(b"{\"Err\":\"nope\"}\n")
        }
        .windows()
        .unwrap_err()
        .message,
        "niri IPC refused the request: nope"
    );
}

#[test]
fn list_apps_dedups_app_ids() {
    let world = world();
    let ids: Vec<_> = world
        .platform
        .list_apps()
        .unwrap()
        .into_iter()
        .map(|app| app.id)
        .collect();
    assert_eq!(ids, ["org.gnome.TextEditor", "foot", "org.example.Floaty"]);
}

#[test]
fn resolve_orders_focused_then_recent_and_casefolds() {
    let world = world();
    world.niri.focus(20);
    let ids = |spec: &AppSpec| -> Vec<i64> {
        world
            .platform
            .resolve(spec)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.window_id)
            .collect()
    };
    assert_eq!(ids(&AppSpec::text("ORG.GNOME.TEXTEDITOR")), [10, 11]);
    assert_eq!(ids(&AppSpec::dict("bundle_id", "foot")), [20]);
    assert!(ids(&AppSpec::text("nope")).is_empty());
    for bad in [AppSpec::text(""), AppSpec::dict("path", "/x")] {
        assert_eq!(
            world.platform.resolve(&bad).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}

#[test]
fn geometry_is_known_for_floating_windows_on_active_workspaces() {
    let world = world();
    let niri = &world.platform.niri;
    let geometry = |id| niri.geometry(&niri.window(id).unwrap().unwrap()).unwrap();
    let editor = geometry(10);
    assert_eq!(editor.rect(), Some(Rect::new(104.0, 56.0, 800.0, 600.0)));
    assert_eq!(editor.output.as_deref(), Some("eDP-1"));
    let floaty = geometry(30);
    assert_eq!(floaty.rect(), Some(Rect::new(1932.0, 23.0, 400.0, 300.0)));
    assert_eq!(
        floaty.output_rect,
        Some(Rect::new(1920.0, 0.0, 2560.0, 1440.0))
    );
    let tiled = geometry(11);
    assert_eq!(tiled.rect(), None);
    assert!(tiled.reason.contains("only for floating windows"));
    world.niri.state().windows[0]["workspace_id"] = json!(3);
    let hidden = geometry(10);
    assert_eq!(hidden.rect(), None);
    assert!(hidden.reason.contains("not on screen"));
}

// --- observation ---------------------------------------------------------------

#[test]
fn observe_maps_atspi_into_the_element_contract() {
    let world = world();
    let observation = world.platform.observe(10).unwrap();
    assert_eq!(
        observation.window_title.as_deref(),
        Some("Doc - Text Editor")
    );
    assert_eq!(
        observation.window_rect,
        Some(Rect::new(104.0, 56.0, 800.0, 600.0))
    );
    assert_eq!(observation.window_id, Some(10));
    assert!(!observation.truncated);
    assert_eq!(
        observation.refs.len(),
        5,
        "the hidden menu and its item are skipped"
    );
    assert_eq!(
        observation.tree[0],
        Element {
            role: Some("push button".to_string()),
            title: Some("Save".to_string()),
            actions: vec!["click".to_string()],
            position: Some((10.0, 10.0)),
            size: Some((80.0, 30.0)),
            ..Element::default()
        }
    );
    assert_eq!(
        observation.tree[1].value.as_deref(),
        Some("hello world hello")
    );
    assert_eq!(observation.tree[2].role.as_deref(), Some("password text"));
    assert_eq!(observation.tree[2].value, None);
    assert_eq!(
        observation.tree[3].children[0].value, None,
        "a label's text repeats its name"
    );
    assert_eq!(observation.focused_index, Some(1));
}

#[test]
fn a_password_value_is_never_read_or_rendered() {
    let app = editor_app();
    let password = app.child(0).child(2);
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![app]), &["grim"]);
    let observation = world.platform.observe(10).unwrap();
    assert!(password.get().reads.is_empty());
    let lines = serialize(&observation.tree);
    assert!(
        lines.contains(&"[2] password text 'Password' [secure] @ (100, 50) 200x30".to_string()),
        "{lines:?}"
    );
    assert!(!lines.join("\n").contains("hunter2"));
}

#[test]
fn the_frame_matches_by_title_and_an_app_off_the_bus_observes_empty() {
    let world = world();
    let other = world.platform.observe(11).unwrap();
    assert_eq!(
        (
            other.window_title.as_deref(),
            other.tree.len(),
            other.window_rect
        ),
        (Some("Other doc"), 0, None)
    );
    let shell = world.platform.observe(20).unwrap();
    assert_eq!(
        (
            shell.window_title.as_deref(),
            shell.tree.len(),
            shell.refs.len()
        ),
        (Some("shell"), 0, 0)
    );
    assert_eq!(
        world.platform.observe(999).unwrap_err().code,
        ErrorCode::AppNotRunning
    );
}

#[test]
fn the_walk_caps_depth_elements_and_time() {
    let mut node = Node::new("PANEL", Some("leaf"));
    for depth in 0..20 {
        let child = node;
        node = Node::new("PANEL", Some(&format!("n{depth}"))).with(|a| a.children = vec![child]);
    }
    let deep = Node::new("APPLICATION", Some("deep")).with(|a| {
        a.children =
            vec![Node::new("FRAME", Some("Floaty")).with(|frame| frame.children = vec![node])];
        a.pid = Some(700);
    });
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![deep]), &[]);
    let observation = world.platform.observe(30).unwrap();
    assert_eq!(observation.refs.len(), MAX_DEPTH);
    assert!(observation.truncated);
    let wide = Node::new("APPLICATION", Some("wide")).with(|a| {
        a.children = vec![Node::new("FRAME", Some("Floaty")).with(|frame| {
            frame.children = (0..1600)
                .map(|i| Node::new("LABEL", Some(&format!("l{i}"))))
                .collect();
        })];
        a.pid = Some(700);
    });
    let wide_world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![wide]), &[]);
    let observation = wide_world.platform.observe(30).unwrap();
    assert_eq!(observation.refs.len(), MAX_ELEMENTS);
    assert!(observation.truncated);
    let world = self::world();
    let app = world.platform.bus().apps[0].clone();
    let frame = app.child(0);
    let walk = world.platform.accessibility.walk(
        &frame,
        Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
    );
    assert!(walk.refs.is_empty() && walk.truncated);
}

#[test]
fn an_unreachable_bus_is_a_transport_error_naming_the_fix() {
    let world = world();
    *world.platform.bus().unavailable.lock().unwrap() =
        Some("AT-SPI: the accessibility bus is unreachable".to_string());
    let error = world.platform.observe(10).unwrap_err();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(error.message.contains("accessibility bus"));
}

// --- secure focus --------------------------------------------------------------

fn editor_with_focus_on(index: usize) -> Node {
    let app = editor_app();
    let frame = app.child(0);
    frame.child(1).get().states.remove("FOCUSED");
    frame.child(index).get().states.insert("FOCUSED");
    app
}

#[test]
fn the_focus_on_an_entry_is_not_secure_and_on_a_password_field_is() {
    assert_eq!(world().platform.focus_security(10), Security::NotSecure);
    let app = editor_with_focus_on(2);
    let password = app.child(0).child(2);
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![app]), &[]);
    assert_eq!(world.platform.focus_security(10), Security::Secure);
    assert!(password.get().reads.is_empty());
    let app = editor_app();
    app.child(0).child(1).get().states.remove("FOCUSED");
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![app]), &[]);
    assert_eq!(
        world.platform.focus_security(10),
        Security::NotSecure,
        "no focus after a complete search"
    );
}

#[test]
fn an_unverifiable_focus_fails_closed() {
    let world = world();
    assert_eq!(
        world.platform.focus_security(20),
        Security::Unverifiable,
        "off the bus"
    );
    assert_eq!(
        world.platform.focus_security(999),
        Security::Unverifiable,
        "window gone"
    );
    let app = editor_app();
    app.child(0).child(1).get().role_fails = true;
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![app]), &[]);
    assert_eq!(world.platform.focus_security(10), Security::Unverifiable);
    let world = self::world();
    *world.platform.bus().unavailable.lock().unwrap() = Some("no bus".to_string());
    assert_eq!(world.platform.focus_security(10), Security::Unverifiable);
}

#[test]
fn a_search_stopped_by_its_bound_is_unverifiable() {
    let wide = Node::new("APPLICATION", Some("wide")).with(|a| {
        a.children = vec![Node::new("FRAME", Some("Floaty")).with(|frame| {
            frame.children = (0..1600)
                .map(|i| Node::new("LABEL", Some(&format!("l{i}"))))
                .collect();
        })];
        a.pid = Some(700);
    });
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![wide]), &[]);
    assert_eq!(world.platform.focus_security(30), Security::Unverifiable);
}

// --- fingerprints ---------------------------------------------------------------

fn fp(parts: &[Option<&str>]) -> Fingerprint {
    parts
        .iter()
        .map(|part| part.map(ToString::to_string))
        .collect()
}

#[test]
fn the_fingerprint_reads_focus_frame_and_value_head_never_a_secure_value() {
    let world = world();
    assert_eq!(
        world.platform.window_fingerprint(10, Duration::ZERO),
        Some(fp(&[
            Some("10"),
            Some("Doc - Text Editor"),
            Some("5"),
            Some("entry"),
            Some("Search"),
            Some("hello world hello")
        ]))
    );
    let app = editor_with_focus_on(2);
    let password = app.child(0).child(2);
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![app]), &[]);
    assert_eq!(
        world.platform.window_fingerprint(10, Duration::ZERO),
        Some(fp(&[
            Some("10"),
            Some("Doc - Text Editor"),
            Some("5"),
            Some("password text"),
            Some("Password"),
            Some("")
        ]))
    );
    assert!(password.get().reads.is_empty());
}

#[test]
fn the_fingerprint_changes_with_the_value_and_is_none_when_gone() {
    let app = editor_app();
    let search = app.child(0).child(1);
    let world = world_with(FakeNiri::default(), FakeAtSpi::new(vec![app]), &[]);
    let before = world.platform.window_fingerprint(10, Duration::ZERO);
    search.get().text = Some("edited".to_string());
    assert_ne!(
        world.platform.window_fingerprint(10, Duration::ZERO),
        before
    );
    assert_eq!(world.platform.window_fingerprint(999, Duration::ZERO), None);
}

// --- element operations ---------------------------------------------------------

#[test]
fn the_press_action_is_the_first_activation_name() {
    let world = world();
    let actions = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        world
            .platform
            .default_action(&actions(&["expand or contract", "Activate"]))
            .as_deref(),
        Some("Activate")
    );
    assert_eq!(
        world
            .platform
            .default_action(&actions(&["click", "press"]))
            .as_deref(),
        Some("click")
    );
    assert_eq!(
        world
            .platform
            .default_action(&actions(&["expand or contract"])),
        None
    );
}

#[test]
fn actions_run_and_refusals_are_unsupported() {
    let world = world();
    let save = world.platform.bus().apps[0].child(0).child(0);
    world.platform.perform(&save, "click").unwrap();
    let missing = world.platform.perform(&save, "press").unwrap_err();
    save.get().do_action_result = false;
    let refused = world.platform.perform(&save, "click").unwrap_err();
    assert_eq!(save.get().performed, ["click", "click"]);
    assert_eq!(missing.message, "the element no longer exposes press");
    assert_eq!(refused.message, "the element refused click");
}

#[test]
fn text_operations_write_read_and_select() {
    let world = world();
    let frame = world.platform.bus().apps[0].child(0);
    let (save, search) = (frame.child(0), frame.child(1));
    assert!(world.platform.is_settable(&search));
    assert!(!world.platform.is_settable(&save));
    world.platform.set_value(&search, "new").unwrap();
    assert_eq!(
        world.platform.current_value(&search).as_deref(),
        Some("new")
    );
    world.platform.select_range(&search, 1, 2).unwrap();
    world.platform.select_range(&search, 0, 1).unwrap();
    assert_eq!(
        world.platform.set_value(&save, "x").unwrap_err().message,
        "the element does not accept text writes"
    );
    assert_eq!(search.get().writes, ["new"]);
    assert_eq!(search.get().selections, [(1, 3), (0, 1)]);
}

#[test]
fn live_fingerprints_and_secure_reads() {
    let world = world();
    let frame = world.platform.bus().apps[0].child(0);
    let (save, password) = (frame.child(0), frame.child(2));
    let live = |node: &Node| world.platform.live_fingerprint(node).unwrap();
    assert_eq!(
        live(&save),
        (Some("push button".to_string()), Some("Save".to_string()))
    );
    assert_eq!(
        live(&password),
        (
            Some("password text".to_string()),
            Some("Password".to_string())
        )
    );
    assert_eq!(world.platform.live_security(&password), Security::Secure);
    assert_eq!(world.platform.live_security(&save), Security::NotSecure);
    save.get().broken = true;
    assert_eq!(live(&save), (None, None));
    assert_eq!(world.platform.live_security(&save), Security::Unverifiable);
}

// --- input dispatch -------------------------------------------------------------

fn edp() -> PointerTarget {
    PointerTarget {
        output: Some("eDP-1".to_string()),
        width: 1920,
        height: 1200,
    }
}

#[test]
fn a_click_maps_onto_the_output_in_logical_coordinates() {
    let world = world();
    world
        .platform
        .click(30, (100.0, 50.0), MouseButton::Right, 2)
        .unwrap();
    assert_eq!(
        world.niri.actions(),
        [json!({"Action": {"FocusWindow": {"id": 30}}})]
    );
    let hdmi = PointerTarget {
        output: Some("HDMI-A-1".to_string()),
        width: 2560,
        height: 1440,
    };
    assert_eq!(
        world.input.calls(),
        [Input::Click(
            hdmi,
            (112.0, 73.0),
            MouseButton::Right,
            2,
            Some(30)
        )]
    );
}

#[test]
fn an_already_focused_window_is_not_refocused() {
    let world = world();
    world
        .platform
        .click(10, (0.0, 0.0), MouseButton::Left, 1)
        .unwrap();
    assert!(world.niri.actions().is_empty());
    assert_eq!(
        world.input.calls(),
        [Input::Click(
            edp(),
            (104.0, 56.0),
            MouseButton::Left,
            1,
            Some(10)
        )]
    );
}

#[test]
fn tiled_windows_refuse_coordinate_input_without_moving_focus() {
    let world = world();
    let errors = [
        world
            .platform
            .click(11, (1.0, 1.0), MouseButton::Left, 1)
            .unwrap_err(),
        world.platform.drag(11, (1.0, 1.0), (2.0, 2.0)).unwrap_err(),
        world
            .platform
            .scroll(11, ScrollDirection::Down, 1, (1.0, 1.0))
            .unwrap_err(),
    ];
    for error in errors {
        assert_eq!(error.code, ErrorCode::ActionUnsupported);
        assert!(error.message.contains("floating"), "{}", error.message);
    }
    assert!(world.niri.actions().is_empty());
    assert!(world.input.calls().is_empty());
}

#[test]
fn points_outside_the_window_are_invalid() {
    let world = world();
    let error = world
        .platform
        .click(10, (800.0, 1.0), MouseButton::Left, 1)
        .unwrap_err();
    assert_eq!(
        error,
        invalid("point (800.0, 1.0) is outside the window (800x600)")
            .with_details(json!({"point": "(800.0, 1.0)"}))
    );
    assert!(world.input.calls().is_empty());
}

#[test]
fn a_focus_that_does_not_land_refuses_input() {
    let world = world();
    world.niri.state().focus_lands = false;
    let errors = [
        world
            .platform
            .click(30, (1.0, 1.0), MouseButton::Left, 1)
            .unwrap_err(),
        world
            .platform
            .press_key(30, &parse_chord("Return").unwrap())
            .unwrap_err(),
        world.platform.type_text(30, "x").unwrap_err(),
    ];
    for error in errors {
        assert_eq!(error.code, ErrorCode::InjectionFailed);
        assert!(error.message.contains("focus did not land"));
    }
    assert!(world.input.calls().is_empty());
}

#[test]
fn empty_text_types_nothing_and_never_moves_focus() {
    let world = world();
    world.platform.type_text(30, "").unwrap();
    assert!(world.input.calls().is_empty());
    assert!(world.niri.actions().is_empty());
}

#[test]
fn chords_and_text_become_keystrokes() {
    let world = world();
    for chord in ["cmd+shift+s", "ctrl+alt+Delete", "PageDown", "ctrl+."] {
        world
            .platform
            .press_key(10, &parse_chord(chord).unwrap())
            .unwrap();
    }
    world.platform.type_text(10, "Hé 1\n").unwrap();
    let stroke = |keysym: &str, modifiers| KeyStroke {
        keysym: keysym.to_string(),
        modifiers,
    };
    assert_eq!(
        world.input.calls(),
        [
            Input::Keys(vec![stroke("s", 65)], Some(10)),
            Input::Keys(vec![stroke("BackSpace", 12)], Some(10)),
            Input::Keys(vec![stroke("Next", 0)], Some(10)),
            Input::Keys(vec![stroke("U002E", 4)], Some(10)),
            Input::Keys(
                vec![
                    KeyStroke::plain("H"),
                    KeyStroke::plain("U00E9"),
                    KeyStroke::plain("U0020"),
                    KeyStroke::plain("1"),
                    KeyStroke::plain("Return")
                ],
                Some(10)
            ),
        ]
    );
    assert_eq!(
        world.platform.type_text(10, "a\u{1b}").unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn typing_rechecks_the_secure_focus_after_focusing() {
    let floaty = floaty_app();
    let ok = floaty.child(0).child(0);
    let niri = FakeNiri::default();
    let moved = ok;
    niri.state().on_focus = Some(std::sync::Arc::new(move |_| {
        let mut accessible = moved.get();
        accessible.role = "PASSWORD_TEXT";
        accessible.states.insert("FOCUSED");
    }));
    let world = world_with(niri, FakeAtSpi::new(vec![editor_app(), floaty]), &[]);
    let error = world.platform.type_text(30, "secret").unwrap_err();
    assert_eq!(error.code, ErrorCode::ActionUnsupported);
    assert_eq!(error.details, Some(json!({"live": true})));
    assert!(world.input.calls().is_empty());
}

#[test]
fn drags_and_scrolls_map_both_points() {
    let world = world();
    world.platform.drag(10, (1.0, 2.0), (3.0, 4.0)).unwrap();
    world
        .platform
        .scroll(10, ScrollDirection::Up, 2, (5.0, 6.0))
        .unwrap();
    assert_eq!(
        world.input.calls(),
        [
            Input::Drag(edp(), (105.0, 58.0), (107.0, 60.0), Some(10)),
            Input::Scroll(edp(), (109.0, 62.0), ScrollDirection::Up, 20, Some(10)),
        ]
    );
}

// --- screenshots -----------------------------------------------------------------

#[test]
fn grim_captures_the_logical_rect_into_the_hardened_dir() {
    use std::os::unix::fs::PermissionsExt;
    let world = world();
    world.script.on_png(&["-g"], (1600, 1200));
    let captured = world.platform.capture(CaptureRequest::Window(10)).unwrap();
    assert_eq!(
        world.script.calls(),
        [[
            "/usr/bin/grim",
            "-g",
            "104,56 800x600",
            captured.path.as_str()
        ]]
    );
    assert_eq!((captured.width, captured.height), (1600, 1200));
    assert_eq!(
        captured.logical_rect,
        Some(Rect::new(104.0, 56.0, 800.0, 600.0))
    );
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(std::path::Path::new(&captured.path)), 0o600);
    assert_eq!(
        mode(std::path::Path::new(&captured.path).parent().unwrap()),
        0o700
    );
}

#[test]
fn tiled_or_overlapped_windows_refuse_and_a_focused_floating_window_is_on_top() {
    let niri = FakeNiri::default();
    niri.state().windows.push(niri_window(
        12,
        "x",
        "over",
        9,
        1,
        false,
        Some((500.0, 300.0)),
        (0.0, 0.0),
        (100, 100),
        1,
    ));
    let world = world_with(niri, FakeAtSpi::default(), &["grim"]);
    world.script.on_png(&["-g"], (10, 10));
    let tiled = world
        .platform
        .capture(CaptureRequest::Window(11))
        .unwrap_err();
    assert_eq!(tiled.code, ErrorCode::ActionUnsupported);
    world.platform.capture(CaptureRequest::Window(10)).unwrap();
    assert_eq!(
        world.script.calls().len(),
        1,
        "the focused window is drawn on top"
    );
    world.niri.focus(20);
    let overlapped = world
        .platform
        .capture(CaptureRequest::Window(10))
        .unwrap_err();
    assert!(overlapped.message.contains("overlaps"));
    assert_eq!(world.script.calls().len(), 1);
}

#[test]
fn grim_missing_failing_or_writing_nothing_is_a_transport_error() {
    let world = world_with(FakeNiri::default(), FakeAtSpi::default(), &[]);
    let missing = world
        .platform
        .capture(CaptureRequest::Window(10))
        .unwrap_err();
    assert!(missing.message.contains("grim"));
    let world = self::world();
    world.script.on(
        &["-g"],
        1,
        b"",
        b"compositor doesn't support wlr-screencopy",
    );
    let failed = world
        .platform
        .capture(CaptureRequest::Window(10))
        .unwrap_err();
    assert!(failed.message.contains("screencopy"));
    let world = self::world();
    assert_eq!(
        world
            .platform
            .capture(CaptureRequest::Window(10))
            .unwrap_err()
            .code,
        ErrorCode::TransportError
    );
}

// --- lock and status -------------------------------------------------------------

#[test]
fn the_lock_reads_logind() {
    assert!(!world().platform.screen_locked());
    assert!(world_with(FakeNiri::default(), FakeAtSpi::default(), &[])
        .platform
        .screen_locked());
}

#[test]
fn the_status_reports_real_capabilities_and_names_missing_pieces() {
    let status = world().platform.permissions().to_json();
    assert_eq!(
        (
            status["accessibility"].clone(),
            status["screen_recording"].clone()
        ),
        (json!("ok"), json!("ok"))
    );
    assert_eq!(status["input"], json!({"pointer": "ok", "keyboard": "ok"}));
    assert!(status["help"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_str()
        .unwrap()
        .contains("floating"));
    let world = world_with(FakeNiri::default(), FakeAtSpi::default(), &[]);
    *world.platform.bus().unavailable.lock().unwrap() =
        Some("AT-SPI: the accessibility bus is unreachable".to_string());
    *world.input.unavailable.lock().unwrap() = Some(unsupported("no socket"));
    let status = world.platform.permissions().to_json();
    assert_eq!(
        (
            status["accessibility"].clone(),
            status["screen_recording"].clone()
        ),
        (json!("missing"), json!("missing"))
    );
    assert_eq!(
        status["input"],
        json!({"pointer": "unknown", "keyboard": "unknown"})
    );
    let help = status["help"].to_string();
    assert!(
        help.contains("accessibility bus")
            && help.contains("grim")
            && help.contains("zwlr_virtual_pointer_manager_v1")
    );
}

// --- the App layer on Wayland ------------------------------------------------------

fn app_env(niri: FakeNiri, atspi: FakeAtSpi, allowed: &[&str]) -> (Env<Fake>, World) {
    let world = world_with(niri, atspi, &["grim", "loginctl"]);
    let platform = WaylandPlatform::new(
        world.script.clone(),
        world.niri.clone(),
        world.platform.bus().clone(),
        world.input.clone(),
        CaptureDir::new(
            world.shots.path().join("shots"),
            world.shots.path().join("home"),
        ),
        Duration::from_millis(50),
    );
    (Env::with_platform(platform, allowed), world)
}

fn default_app_env() -> (Env<Fake>, World) {
    app_env(
        FakeNiri::default(),
        FakeAtSpi::default(),
        &["org.gnome.TextEditor", "org.example.Floaty", "foot"],
    )
}

fn bind(env: &Env<Fake>, spec: &str) -> crate::session::BoundApp {
    env.session.get_app(&AppSpec::text(spec), None).unwrap()
}

fn click(index: i64, button: MouseButton) -> AppCall {
    AppCall::Click {
        target: TargetArg::Index(index),
        button,
        count: 1,
    }
}

fn point(x: f64, y: f64) -> AppCall {
    AppCall::Click {
        target: TargetArg::Point(PointArg::Valid {
            x,
            y,
            repr: format!("({x:?}, {y:?})"),
        }),
        button: MouseButton::Left,
        count: 1,
    }
}

#[test]
fn get_app_binds_the_focused_window_and_observes() {
    let (env, _world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    assert_eq!(
        (app.bundle_id.as_str(), app.pid),
        ("org.gnome.TextEditor", 10)
    );
    let state = app.state.clone().unwrap();
    assert!(
        state.contains("org.gnome.TextEditor (org.gnome.TextEditor) — window 'Doc - Text Editor'"),
        "{state}"
    );
    assert!(
        state.contains("[0] push button 'Save' (actions: click) @ (10, 10) 80x30"),
        "{state}"
    );
    assert!(state.contains("[secure]"));
    assert_eq!(bind(&env, "org.gnome.TextEditor"), app);
}

#[test]
fn get_app_gates_and_reports_missing_apps() {
    let (env, _world) = app_env(FakeNiri::default(), FakeAtSpi::default(), &["foot"]);
    assert_eq!(
        env.session
            .get_app(&AppSpec::text("org.gnome.TextEditor"), None)
            .unwrap_err()
            .code,
        ErrorCode::AppNotAllowed
    );
    let missing = env
        .session
        .get_app(&AppSpec::text("org.nope"), None)
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::AppNotRunning);
    assert!(missing.message.contains("in the niri session"));
}

#[test]
fn an_element_click_runs_the_atspi_action_without_focus_or_pointer() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    env.session
        .call(app.handle, click(0, MouseButton::Left))
        .unwrap();
    assert_eq!(
        world.platform.bus().apps[0]
            .child(0)
            .child(0)
            .get()
            .performed,
        ["click"]
    );
    assert!(world.input.calls().is_empty());
    assert!(world.niri.actions().is_empty());
}

#[test]
fn an_element_without_an_action_clicks_its_center_through_the_pointer() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    env.session
        .call(app.handle, click(1, MouseButton::Left))
        .unwrap();
    env.session
        .call(app.handle, click(0, MouseButton::Right))
        .unwrap();
    assert_eq!(
        world.input.calls(),
        [
            Input::Click(edp(), (304.0, 81.0), MouseButton::Left, 1, Some(10)),
            Input::Click(edp(), (154.0, 81.0), MouseButton::Right, 1, Some(10)),
        ]
    );
}

fn editor_with_password(grab_focus: Option<bool>) -> (FakeAtSpi, Node) {
    let atspi = FakeAtSpi::default();
    let frame = atspi.apps[0].child(0);
    let password = frame.child(2);
    {
        let mut accessible = password.get();
        accessible.actions = vec!["activate".to_string()];
        accessible.grab_focus = grab_focus;
    }
    frame.child(1).get().states.remove("FOCUSED");
    (atspi, password)
}

#[test]
fn clicking_a_field_focuses_it_through_grab_focus() {
    let (atspi, password) = editor_with_password(Some(true));
    let (env, world) = app_env(FakeNiri::default(), atspi, &["org.gnome.TextEditor"]);
    let app = bind(&env, "org.gnome.TextEditor");
    env.session
        .call(app.handle, click(2, MouseButton::Left))
        .unwrap();
    assert!(password.get().states.contains("FOCUSED"));
    assert!(
        password.get().performed.is_empty(),
        "a field's activate (Enter) never stands in for a click"
    );
    assert!(world.input.calls().is_empty());
}

#[test]
fn without_grab_focus_a_field_click_is_a_real_pointer_click() {
    let (atspi, password) = editor_with_password(Some(false));
    let (env, world) = app_env(FakeNiri::default(), atspi, &["org.gnome.TextEditor"]);
    let focused = password.clone();
    *world.input.on_click.lock().unwrap() = Some(Box::new(move || {
        focused.get().states.insert("FOCUSED");
    }));
    let app = bind(&env, "org.gnome.TextEditor");
    env.session
        .call(app.handle, click(2, MouseButton::Left))
        .unwrap();
    assert!(password.get().performed.is_empty());
    assert!(matches!(world.input.calls()[..], [Input::Click(..)]));
    assert!(password.get().states.contains("FOCUSED"));
}

#[test]
fn a_field_that_never_takes_focus_fails_the_click() {
    let (atspi, password) = editor_with_password(Some(false));
    let (env, _world) = app_env(FakeNiri::default(), atspi, &["org.gnome.TextEditor"]);
    let app = bind(&env, "org.gnome.TextEditor");
    let error = env
        .session
        .call(app.handle, click(2, MouseButton::Left))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InjectionFailed);
    assert!(
        password.get().performed.is_empty(),
        "never silently activates instead"
    );
}

#[test]
fn screenshot_points_scale_back_to_logical() {
    let (env, world) = default_app_env();
    world.script.on_png(&["-g"], (1600, 1200));
    let app = bind(&env, "org.gnome.TextEditor");
    env.session.call(app.handle, point(10.0, 20.0)).unwrap();
    let shot = env
        .session
        .call(app.handle, AppCall::GetScreenshot)
        .unwrap();
    env.session.call(app.handle, point(200.0, 100.0)).unwrap();
    let outside = env
        .session
        .call(app.handle, point(1600.0, 5.0))
        .unwrap_err();
    assert_eq!(
        (shot["width"].clone(), shot["height"].clone()),
        (json!(1600), json!(1200))
    );
    let points: Vec<Pair> = world
        .input
        .calls()
        .into_iter()
        .map(|call| match call {
            Input::Click(_, point, ..) => point,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(points, [(114.0, 76.0), (204.0, 106.0)]);
    assert_eq!(outside.code, ErrorCode::InvalidArgument);
}

#[test]
fn keyboard_flows_focus_the_window_and_refuse_a_secure_focus() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.example.Floaty");
    env.session
        .call(
            app.handle,
            AppCall::PressKey {
                key: TextArg::Text("ctrl+s".to_string()),
            },
        )
        .unwrap();
    env.session
        .call(
            app.handle,
            AppCall::TypeText {
                text: TextArg::Text("ok".to_string()),
            },
        )
        .unwrap();
    assert_eq!(
        world.niri.actions(),
        [json!({"Action": {"FocusWindow": {"id": 30}}})]
    );
    assert_eq!(world.input.calls().len(), 2);
    // The focused OK button turns into a password field.
    let ok = world.platform.bus().apps[1].child(0).child(0);
    ok.get().states.insert("FOCUSED");
    ok.get().role = "PASSWORD_TEXT";
    let secure = env
        .session
        .call(
            app.handle,
            AppCall::TypeText {
                text: TextArg::Text("hunter2".to_string()),
            },
        )
        .unwrap_err();
    assert_eq!(secure.details, Some(json!({"live": true})));
    ok.get().role_fails = true;
    let unknown = env
        .session
        .call(
            app.handle,
            AppCall::PressKey {
                key: TextArg::Text("Return".to_string()),
            },
        )
        .unwrap_err();
    assert_eq!(unknown.details, Some(json!({"live": false})));
    assert_eq!(world.input.calls().len(), 2);
}

#[test]
fn set_value_select_text_and_secondary_actions_use_atspi() {
    let (env, world) = default_app_env();
    let frame = world.platform.bus().apps[0].child(0);
    let app = bind(&env, "org.gnome.TextEditor");
    let call = |call| env.session.call(app.handle, call);
    call(AppCall::SelectText {
        index: IndexArg::Valid(1),
        text: "world".to_string(),
        prefix: None,
        suffix: None,
    })
    .unwrap();
    call(AppCall::SetValue {
        index: IndexArg::Valid(1),
        value: "line one\nline two".to_string(),
    })
    .unwrap();
    let secondary = |name: &str| AppCall::SecondaryAction {
        index: IndexArg::Valid(0),
        action: ActionArg::Name(name.to_string()),
    };
    call(secondary("click")).unwrap();
    let secure = call(AppCall::SetValue {
        index: IndexArg::Valid(2),
        value: "hunter2".to_string(),
    })
    .unwrap_err();
    let not_editable = call(AppCall::SetValue {
        index: IndexArg::Valid(0),
        value: "x".to_string(),
    })
    .unwrap_err();
    let unexposed = call(secondary("press")).unwrap_err();
    assert_eq!(frame.child(1).get().selections, [(6, 11)]);
    assert_eq!(frame.child(1).get().writes, ["line one\nline two"]);
    assert_eq!(frame.child(0).get().performed, ["click"]);
    assert!(frame.child(2).get().writes.is_empty());
    assert_eq!(
        secure.details,
        Some(json!({"element_index": 2, "secure": true}))
    );
    assert_eq!(not_editable.code, ErrorCode::ActionUnsupported);
    assert_eq!(unexposed.code, ErrorCode::ActionUnsupported);
}

#[test]
fn paste_and_ocr_name_the_wayland_gap() {
    let (env, _world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    let paste = env
        .session
        .call(
            app.handle,
            AppCall::Paste {
                text: "x".to_string(),
                format: crate::platform::PasteFormat::Text,
            },
        )
        .unwrap_err();
    assert_eq!(paste.code, ErrorCode::ActionUnsupported);
    assert!(paste.message.contains("Wayland backend"));
    assert_eq!(
        env.actions().last(),
        Some(&("paste", Outcome::Error(ErrorCode::ActionUnsupported)))
    );
    assert_eq!(
        env.session
            .call(app.handle, AppCall::GetTextRegions)
            .unwrap_err()
            .code,
        ErrorCode::ActionUnsupported
    );
}

#[test]
fn activate_and_is_frontmost_go_through_niri() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.example.Floaty");
    assert_eq!(
        env.session.call(app.handle, AppCall::IsFrontmost).unwrap(),
        json!(false)
    );
    env.session.call(app.handle, AppCall::Activate).unwrap();
    assert_eq!(
        env.session.call(app.handle, AppCall::IsFrontmost).unwrap(),
        json!(true)
    );
    assert_eq!(
        world.niri.actions(),
        [json!({"Action": {"FocusWindow": {"id": 30}}})]
    );
}

#[test]
fn the_guard_rejects_a_gone_or_reassigned_window() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    world.niri.state().windows[0]["app_id"] = json!("org.other");
    assert_eq!(
        env.session
            .call(app.handle, AppCall::GetAxState { diff: true })
            .unwrap_err()
            .code,
        ErrorCode::AppNotRunning
    );
    world.niri.state().windows.remove(0);
    assert_eq!(
        env.session
            .call(app.handle, click(0, MouseButton::Left))
            .unwrap_err()
            .code,
        ErrorCode::AppNotRunning
    );
}

#[test]
fn a_renamed_element_is_stale() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    world.platform.bus().apps[0].child(0).child(0).get().name = Some("Save As".to_string());
    assert_eq!(
        env.session
            .call(app.handle, click(0, MouseButton::Left))
            .unwrap_err()
            .code,
        ErrorCode::ElementStale
    );
}

#[test]
fn actions_settle_on_the_wayland_fingerprint() {
    let (env, world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    let before = world.niri.state().requests.len();
    env.session
        .call(app.handle, click(0, MouseButton::Left))
        .unwrap();
    let windows_reads = world.niri.state().requests[before..]
        .iter()
        .filter(|request| request.as_str() == Some("FocusedWindow"))
        .count();
    assert!(
        windows_reads >= 2,
        "two fingerprint reads, each reading niri's focus"
    );
}

#[test]
fn diffing_state_and_permissions_report_wayland() {
    let (env, _world) = default_app_env();
    let app = bind(&env, "org.gnome.TextEditor");
    assert_eq!(
        env.session
            .call(app.handle, AppCall::GetAxState { diff: true })
            .unwrap(),
        json!("(no changes since the previous observation)")
    );
    let state = env.session.get_state(false).unwrap();
    assert_eq!(state["platform"], json!("wayland"));
    assert_eq!(state["apps"][0]["id"], json!("org.gnome.TextEditor"));
    let status = env.session.permissions();
    assert_eq!(status["accessibility"], json!("ok"));
    assert_eq!(status["input"], json!({"pointer": "ok", "keyboard": "ok"}));
}
