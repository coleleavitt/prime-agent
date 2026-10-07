//! Ported from the skill's `tests/test_ax_security.py` (the AX reads),
//! `tests/test_capture_security.py` (the screencapture argv and error
//! halves, and the inject error contract), the `apps.py` and `inject.py`
//! behaviour `tests/test_api.py` drove through `fakes.py`, plus the App
//! layer end to end on the macOS backend.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use serde_json::json;

use super::ax::{Accessibility, AxValue, MESSAGING_TIMEOUT};
use super::events::{MacEvent, MouseKind};
use super::fakes::{geometry, workspace_app, AxCall, FakeAx, FakeDesktop, Node, CANNOT_COMPLETE};
use super::*;
use crate::element::{Element, MAX_ACTIONS, MAX_ELEMENTS};
use crate::error::ErrorCode;
use crate::keymap::{parse_chord, Modifier};
use crate::process::script::Script;
use crate::session::fake::Env;
use crate::session::{AppCall, TargetArg};
use crate::spec::AppSpec;

type Fake = MacPlatform<Script, FakeAx, FakeDesktop>;

struct World {
    platform: Fake,
    ax: FakeAx,
    desktop: FakeDesktop,
    script: Script,
    shots: tempfile::TempDir,
}

fn world_with(ax: FakeAx) -> World {
    let script = Script::default();
    let desktop = FakeDesktop::default();
    let shots = tempfile::tempdir().unwrap();
    let capture = CaptureDir::new(
        shots.path().join("shots"),
        shots.path().join("unrelated-home"),
    );
    World {
        platform: MacPlatform::new(script.clone(), ax.clone(), desktop.clone(), capture)
            .with_appear_timeout(Duration::from_millis(40), Duration::from_millis(1)),
        ax,
        desktop,
        script,
        shots,
    }
}

fn world() -> World {
    world_with(FakeAx::default())
}

/// An app (pid 4242) whose focused window "Main" holds one group.
fn main_window() -> (Node, Node, Node) {
    let child = Node::new(3).text("AXRole", "AXGroup").null("AXSubrole");
    let window = Node::new(2)
        .text("AXRole", "AXWindow")
        .text("AXTitle", "Main")
        .geometry((100.0, 50.0), (400.0, 300.0))
        .children(vec![child.clone()]);
    window.set("_AXWindowID", Ok(AxValue::Integer(7)));
    let app = Node::new(1).null("AXFocusedUIElement");
    app.set("AXFocusedWindow", Ok(AxValue::Element(window.clone())));
    (app, window, child)
}

fn over(app: Node) -> (Accessibility<FakeAx>, FakeAx) {
    let ax = FakeAx::with_app(4242, app);
    (Accessibility { ax: ax.clone() }, ax)
}

// --- AX reads ----------------------------------------------------------------------

#[test]
fn geometry_unwraps_the_ax_value_refs_and_other_values_are_no_point() {
    let element = Node::new(9)
        .text("AXRole", "AXButton")
        .null("AXSubrole")
        .geometry((10.0, 20.0), (30.0, 40.0));
    let (accessibility, _) = over(Node::new(1));
    let described = accessibility.describe(&element, None);
    assert_eq!(
        (described.position, described.size),
        (Some((10.0, 20.0)), Some((30.0, 40.0)))
    );
    element.set("AXPosition", Ok(AxValue::Text("not geometry".to_string())));
    element.set("AXSize", Ok(AxValue::Null));
    let described = accessibility.describe(&element, None);
    assert_eq!((described.position, described.size), (None, None));
}

#[test]
fn observe_bounds_the_app_window_and_child_refs() {
    let (app, _window, _child) = main_window();
    let (accessibility, ax) = over(app);
    let observation = accessibility.observe(4242, |_| None);
    assert_eq!(observation.window_title.as_deref(), Some("Main"));
    assert_eq!(observation.window_id, Some(7));
    assert_eq!(
        observation.window_rect,
        Some(Rect::new(100.0, 50.0, 400.0, 300.0))
    );
    assert_eq!(
        observation.tree,
        [Element {
            role: Some("AXGroup".to_string()),
            ..Element::default()
        }]
    );
    assert!(!observation.truncated);
    let bounded: BTreeSet<u32> = ax.timed_out_elements().into_iter().collect();
    assert_eq!(bounded, BTreeSet::from([1, 2, 3]));
    // Every read through a reference is preceded by its timeout.
    let calls = ax.calls();
    for (index, call) in calls.iter().enumerate() {
        if let AxCall::Read(element, _) = call {
            assert!(
                calls[..index].iter().any(
                    |earlier| matches!(earlier, AxCall::Timeout(bounded, _) if bounded == element)
                ),
                "read before its timeout: {call:?}"
            );
        }
    }
}

#[test]
fn a_windowless_app_observes_empty() {
    let app = Node::new(1);
    app.set(
        "AXFocusedWindow",
        Ok(AxValue::Element(
            Node::new(1).text("AXRole", "AXApplication"),
        )),
    );
    let (accessibility, _) = over(app);
    assert_eq!(accessibility.observe(4242, |_| None), Observation::empty());
    let (accessibility, _) = accessibility_with_failed_focus();
    assert_eq!(accessibility.observe(4242, |_| None), Observation::empty());
}

fn accessibility_with_failed_focus() -> (Accessibility<FakeAx>, FakeAx) {
    over(Node::new(1).fails("AXFocusedWindow"))
}

#[test]
fn the_window_server_supplies_a_missing_rect_by_window_id() {
    let (app, window, _) = main_window();
    window.set("AXPosition", Ok(AxValue::Null));
    let world = world_with(FakeAx::with_app(4242, app));
    // The front window is someone else's: the bounds come from the match
    // on the CGWindowID, not from the first on-screen entry.
    world.desktop.state().windows = Some(vec![
        (99, Rect::new(0.0, 0.0, 10.0, 10.0)),
        (7, Rect::new(5.0, 6.0, 700.0, 500.0)),
    ]);
    let observation = world.platform.observe(4242).unwrap();
    assert_eq!(
        observation.window_rect,
        Some(Rect::new(5.0, 6.0, 700.0, 500.0))
    );
    world.desktop.state().windows = Some(vec![(99, Rect::new(0.0, 0.0, 10.0, 10.0))]);
    assert_eq!(world.platform.observe(4242).unwrap().window_rect, None);
}

#[test]
fn without_a_window_id_or_a_readable_window_list_there_is_no_rect() {
    let (app, window, _) = main_window();
    window.set("AXPosition", Ok(AxValue::Null));
    window.set("_AXWindowID", Ok(AxValue::Null));
    let world = world_with(FakeAx::with_app(4242, app));
    world.desktop.state().windows = Some(vec![(7, Rect::new(1.0, 2.0, 3.0, 4.0))]);
    assert_eq!(world.platform.observe(4242).unwrap().window_rect, None);
    assert_eq!(world.desktop.state().window_reads, 0);
    window.set("_AXWindowID", Ok(AxValue::Integer(7)));
    world.desktop.state().windows = None;
    let observation = world.platform.observe(4242).unwrap();
    assert_eq!(
        (observation.window_rect, observation.window_id),
        (None, Some(7))
    );
}

#[test]
fn an_observation_cut_by_the_element_cap_is_marked_truncated() {
    let wide: Vec<Node> = (0..1600)
        .map(|index| Node::new(100 + index).text("AXRole", "AXStaticText"))
        .collect();
    let (app, window, _) = main_window();
    window.set("AXChildren", Ok(AxValue::Elements(wide)));
    let observation = over(app).0.observe(4242, |_| None);
    assert_eq!(
        (observation.refs.len(), observation.truncated),
        (MAX_ELEMENTS, true)
    );
}

#[test]
fn every_described_string_and_action_name_is_capped() {
    let long = "x".repeat(5000);
    let element = Node::new(5)
        .actions(Ok(vec![long.clone(), "AXPress".to_string()]))
        .otherwise(Ok(AxValue::Text(long)));
    let described = over(Node::new(1)).0.describe(&element, None);
    let capped = format!("{}…", "x".repeat(2000));
    for text in [
        &described.role,
        &described.subrole,
        &described.title,
        &described.value,
        &described.description,
        &described.placeholder,
    ] {
        assert_eq!(text.as_deref(), Some(capped.as_str()));
    }
    assert_eq!(described.actions, [capped, "AXPress".to_string()]);
}

#[test]
fn the_walk_stops_at_the_deadline_without_reading() {
    let root = Node::new(5).children(vec![Node::new(6)]);
    let (accessibility, ax) = over(Node::new(1));
    let elapsed = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let (tree, refs, stopped) = accessibility.walk_until(&root, elapsed);
    assert!(tree.is_empty() && refs.is_empty() && stopped);
    assert!(ax.reads().is_empty());
}

#[test]
fn the_walk_caps_elements_and_prunes_an_element_that_is_its_own_child() {
    let wide: Vec<Node> = (0..1600)
        .map(|index| Node::new(100 + index).text("AXRole", "AXStaticText"))
        .collect();
    let root = Node::new(5).children(wide);
    let (accessibility, _) = over(Node::new(1));
    let (_, refs, stopped) =
        accessibility.walk_until(&root, Instant::now() + Duration::from_secs(60));
    assert_eq!((refs.len(), stopped), (MAX_ELEMENTS, true));

    let looping = Node::new(5).text("AXRole", "AXGroup");
    looping.set(
        "AXChildren",
        Ok(AxValue::Elements(vec![
            looping.clone(),
            Node::new(6).text("AXRole", "AXButton"),
        ])),
    );
    let (tree, refs, stopped) =
        accessibility.walk_until(&looping, Instant::now() + Duration::from_secs(60));
    assert_eq!((refs.len(), refs[0].0.element), (1, 6));
    assert_eq!(
        (tree.len(), tree[0].role.as_deref(), stopped),
        (1, Some("AXButton"), false)
    );
}

#[test]
fn the_focused_element_is_found_by_cf_equality() {
    let (app, _, child) = main_window();
    app.set("AXFocusedUIElement", Ok(AxValue::Element(child.alias())));
    let (accessibility, _) = over(app);
    assert_eq!(accessibility.observe(4242, |_| None).focused_index, Some(0));
}

#[test]
fn the_fingerprint_bounds_its_reads() {
    let (app, _, _) = main_window();
    let (accessibility, ax) = over(app);
    let fingerprint = accessibility
        .window_fingerprint(4242, Some(Duration::from_millis(10)))
        .unwrap();
    assert_eq!(
        fingerprint,
        [
            Some("Main".to_string()),
            Some("1".to_string()),
            None,
            None,
            None
        ]
    );
    // A budget below the floor still bounds every read at 50 ms.
    assert!(ax.calls().iter().all(
        |call| !matches!(call, AxCall::Timeout(_, timeout) if *timeout != Duration::from_millis(50))
    ));
    assert_eq!(
        accessibility_with_failed_focus()
            .0
            .window_fingerprint(4242, None),
        None
    );
}

#[test]
fn the_fingerprint_reads_the_value_head_but_never_a_secure_value() {
    let (app, _, _) = main_window();
    let field = Node::new(8)
        .text("AXRole", "AXTextField")
        .null("AXSubrole")
        .text("AXValue", &"v".repeat(500));
    app.set("AXFocusedUIElement", Ok(AxValue::Element(field)));
    let (accessibility, _) = over(app);
    let fingerprint = accessibility.window_fingerprint(4242, None).unwrap();
    assert_eq!(fingerprint[4], Some("v".repeat(200)));

    let window = Node::new(2).fails("AXTitle").fails("AXChildren");
    let secure = Node::new(8)
        .text("AXRole", "AXTextField")
        .text("AXSubrole", "AXSecureTextField")
        .text("AXValue", "hunter2");
    let app = Node::new(1);
    app.set("AXFocusedWindow", Ok(AxValue::Element(window)));
    app.set("AXFocusedUIElement", Ok(AxValue::Element(secure)));
    let (accessibility, ax) = over(app);
    let fingerprint = accessibility.window_fingerprint(4242, None).unwrap();
    assert_eq!(
        fingerprint,
        [
            None,
            Some("0".to_string()),
            Some("AXTextField".to_string()),
            Some("AXSecureTextField".to_string()),
            Some(String::new())
        ]
    );
    assert!(!ax.reads().contains(&"AXValue".to_string()));
}

#[test]
fn an_unverifiable_focused_field_never_has_its_value_read() {
    let (app, _, _) = main_window();
    let field = Node::new(8)
        .text("AXRole", "AXTextField")
        .fails("AXSubrole")
        .text("AXValue", "hunter2");
    app.set("AXFocusedUIElement", Ok(AxValue::Element(field)));
    let (accessibility, ax) = over(app);
    assert_eq!(
        accessibility.window_fingerprint(4242, None).unwrap()[4],
        Some(String::new())
    );
    assert!(!ax.reads().contains(&"AXValue".to_string()));
}

#[test]
fn the_live_focus_read_fails_closed() {
    let (accessibility, _) = over(Node::new(1).fails("AXFocusedUIElement"));
    assert_eq!(accessibility.focused_is_secure(4242), None);
    let (accessibility, _) = over(Node::new(1).null("AXFocusedUIElement"));
    assert_eq!(accessibility.focused_is_secure(4242), Some(false));
    let secure = Node::new(8)
        .text("AXRole", "AXTextField")
        .text("AXSubrole", "AXSecureTextField");
    let app = Node::new(1);
    app.set("AXFocusedUIElement", Ok(AxValue::Element(secure)));
    let (accessibility, ax) = over(app);
    assert_eq!(accessibility.focused_is_secure(4242), Some(true));
    assert!(ax.reads().contains(&"AXSubrole".to_string()));
    let unreadable = Node::new(8)
        .text("AXRole", "AXTextField")
        .fails("AXSubrole");
    let app = Node::new(1);
    app.set("AXFocusedUIElement", Ok(AxValue::Element(unreadable)));
    assert_eq!(accessibility_from(app).focused_is_secure(4242), None);
    assert_eq!(
        accessibility_from(Node::new(1)).focused_is_secure(999),
        None
    );
}

fn accessibility_from(app: Node) -> Accessibility<FakeAx> {
    over(app).0
}

#[test]
fn the_live_field_read_fails_closed_and_reads_the_live_subrole() {
    let accessibility = accessibility_from(Node::new(1));
    assert_eq!(
        accessibility.live_is_secure(&Node::new(5).otherwise(Err(CANNOT_COMPLETE))),
        None
    );
    let secure = Node::new(5)
        .text("AXRole", "AXTextField")
        .text("AXSubrole", "AXSecureTextField");
    assert_eq!(accessibility.live_is_secure(&secure), Some(true));
    let plain = Node::new(5).text("AXRole", "AXTextField").null("AXSubrole");
    assert_eq!(accessibility.live_is_secure(&plain), Some(false));
}

#[test]
fn a_described_field_with_an_unreadable_subrole_never_carries_a_value() {
    let (accessibility, ax) = over(Node::new(1));
    let field = Node::new(5)
        .text("AXRole", "AXTextField")
        .fails("AXSubrole")
        .text("AXValue", "hunter2");
    let described = accessibility.describe(&field, None);
    assert_eq!(described.subrole.as_deref(), Some("AXSecureTextField"));
    assert_eq!(described.value, None);
    assert!(!ax.reads().contains(&"AXValue".to_string()));
    let secure = Node::new(5)
        .text("AXRole", "AXTextField")
        .text("AXSubrole", "AXSecureTextField")
        .text("AXValue", "hunter2");
    assert_eq!(accessibility.describe(&secure, None).value, None);
}

#[test]
fn attributes_titles_and_action_names_are_bounded() {
    let (app, window, _) = main_window();
    window.set("AXTitle", Ok(AxValue::Text("t".repeat(5000))));
    let (accessibility, _) = over(app);
    let title = accessibility.observe(4242, |_| None).window_title.unwrap();
    assert_eq!(title.chars().count(), 2001);
    assert!(title.ends_with('…'));
    let busy = Node::new(5).actions(Ok((0..500)
        .map(|index| format!("AXAction{index}"))
        .collect()));
    let actions = accessibility.actions(&busy, None);
    assert_eq!(
        (actions.len(), actions[0].as_str()),
        (MAX_ACTIONS, "AXAction0")
    );
    assert!(accessibility
        .actions(&Node::new(5).actions(Err(CANNOT_COMPLETE)), None)
        .is_empty());
}

#[test]
fn values_read_as_python_text() {
    let cases: [(AxValue<Node>, Option<&str>); 9] = [
        (AxValue::Text("hi".to_string()), Some("hi")),
        (AxValue::Integer(3), Some("3")),
        (AxValue::Float(0.5), Some("0.5")),
        (AxValue::Float(1.0), Some("1.0")),
        (AxValue::Bool(true), Some("True")),
        (geometry((1.0, 2.0)), Some("<AXValue (1.0, 2.0)>")),
        (
            AxValue::Other("<NSURL file:///>".to_string()),
            Some("<NSURL file:///>"),
        ),
        (AxValue::Null, None),
        (AxValue::Element(Node::new(1)), None),
    ];
    for (value, text) in cases {
        assert_eq!(value.text().as_deref(), text, "{value:?}");
    }
}

#[test]
fn element_operations_report_their_ax_errors() {
    let world = world();
    let field = Node::new(5).settable(Ok(true));
    let platform = &world.platform;
    assert!(platform.is_settable(&field));
    assert!(!platform.is_settable(&Node::new(5).settable(Err(CANNOT_COMPLETE))));
    platform.set_value(&field, "hello").unwrap();
    platform.select_range(&field, 2, 3).unwrap();
    platform.perform(&field, "AXPress").unwrap();
    assert_eq!(
        world
            .ax
            .calls()
            .into_iter()
            .filter(|call| !matches!(call, AxCall::Timeout(..)))
            .collect::<Vec<_>>(),
        [
            AxCall::Settable(5, "AXValue".to_string()),
            AxCall::Settable(5, "AXValue".to_string()),
            AxCall::SetString(5, "AXValue".to_string(), "hello".to_string()),
            AxCall::SetRange(5, "AXSelectedTextRange".to_string(), 2, 3),
            AxCall::Perform(5, "AXPress".to_string()),
        ]
    );
    world.ax.state().perform_error = Some(CANNOT_COMPLETE);
    world.ax.state().write_error = Some(CANNOT_COMPLETE);
    assert_eq!(
        platform.perform(&field, &"AXRaise".repeat(10)).unwrap_err(),
        unsupported(format!(
            "the element did not perform {} (AX error -25204)",
            "AXRaise".repeat(10)
        ))
        .with_details(json!({"action": "AXRaiseAXRaiseAXRaiseAXRaiseAXRa"}))
    );
    assert_eq!(
        platform.set_value(&field, "x").unwrap_err(),
        unsupported("setting the value failed with AX error -25204").with_details(json!({}))
    );
    assert_eq!(
        platform.select_range(&field, 1, 2).unwrap_err(),
        unsupported("selecting the text range failed with AX error -25204")
            .with_details(json!({"location": 1, "length": 2}))
    );
    assert_eq!(
        platform
            .default_action(&["AXShowMenu".to_string(), "AXPress".to_string()])
            .as_deref(),
        Some("AXPress")
    );
    assert_eq!(platform.default_action(&["AXShowMenu".to_string()]), None);
}

use crate::error::unsupported;

// --- input -------------------------------------------------------------------------

fn mouse(kind: MouseKind, button: MouseButton, point: Pair) -> MacEvent {
    MacEvent::Mouse {
        kind,
        button,
        point,
    }
}

#[test]
fn clicks_drags_and_scrolls_post_to_the_pid() {
    let world = world();
    let platform = &world.platform;
    platform
        .click(77, (10.0, 20.0), MouseButton::Right, 2)
        .unwrap();
    platform.drag(77, (1.0, 2.0), (3.0, 4.0)).unwrap();
    platform
        .scroll(77, ScrollDirection::Up, 2, (5.0, 6.0))
        .unwrap();
    platform
        .scroll(77, ScrollDirection::Right, 1, (5.0, 6.0))
        .unwrap();
    let right = MouseButton::Right;
    let left = MouseButton::Left;
    assert_eq!(
        world.desktop.state().posted,
        [
            (
                77,
                vec![
                    mouse(MouseKind::Down, right, (10.0, 20.0)),
                    mouse(MouseKind::Up, right, (10.0, 20.0)),
                    mouse(MouseKind::Down, right, (10.0, 20.0)),
                    mouse(MouseKind::Up, right, (10.0, 20.0)),
                ]
            ),
            (
                77,
                vec![
                    mouse(MouseKind::Down, left, (1.0, 2.0)),
                    mouse(MouseKind::Dragged, left, (3.0, 4.0)),
                    mouse(MouseKind::Up, left, (3.0, 4.0)),
                ]
            ),
            (
                77,
                vec![MacEvent::Scroll {
                    vertical: -1600,
                    horizontal: 0,
                    location: (5.0, 6.0)
                }]
            ),
            (
                77,
                vec![MacEvent::Scroll {
                    vertical: 0,
                    horizontal: 800,
                    location: (5.0, 6.0)
                }]
            ),
        ]
    );
}

#[test]
fn chords_carry_their_flags_on_both_events() {
    let world = world();
    world
        .platform
        .press_key(77, &parse_chord("cmd+shift+f").unwrap())
        .unwrap();
    world
        .platform
        .press_key(77, &parse_chord("Return").unwrap())
        .unwrap();
    let modifiers = BTreeSet::from([Modifier::Cmd, Modifier::Shift]);
    assert_eq!(
        world.desktop.state().posted,
        [
            (
                77,
                vec![
                    MacEvent::Key {
                        keycode: 3,
                        down: true,
                        modifiers: modifiers.clone()
                    },
                    MacEvent::Key {
                        keycode: 3,
                        down: false,
                        modifiers
                    },
                ]
            ),
            (
                77,
                vec![
                    MacEvent::Key {
                        keycode: 36,
                        down: true,
                        modifiers: BTreeSet::new()
                    },
                    MacEvent::Key {
                        keycode: 36,
                        down: false,
                        modifiers: BTreeSet::new()
                    },
                ]
            ),
        ]
    );
}

#[test]
fn typing_sends_two_utf16_units_per_event_never_splitting_a_pair() {
    let world = world();
    world.platform.type_text(77, "abc😀d").unwrap();
    world.platform.type_text(77, "").unwrap();
    let posted = world.desktop.state().posted.clone();
    assert_eq!(posted.len(), 1);
    let chunks: Vec<(bool, String)> = posted[0]
        .1
        .iter()
        .map(|event| match event {
            MacEvent::Unicode { down, units } => (*down, String::from_utf16(units).unwrap()),
            other => panic!("{other:?}"),
        })
        .collect();
    let expected = [
        ("ab", true),
        ("ab", false),
        ("c", true),
        ("c", false),
        ("😀", true),
        ("😀", false),
        ("d", true),
        ("d", false),
    ];
    assert_eq!(
        chunks,
        expected.map(|(text, down)| (down, text.to_string()))
    );
}

#[test]
fn a_failed_event_is_injection_failed_naming_the_action() {
    let world = world();
    world.desktop.state().post_error = Some(PostError("no cg access".to_string()));
    let platform = &world.platform;
    let failures = [
        (
            "click",
            platform.click(123, (1.0, 2.0), MouseButton::Left, 1),
        ),
        ("drag", platform.drag(123, (1.0, 2.0), (3.0, 4.0))),
        (
            "scroll",
            platform.scroll(123, ScrollDirection::Up, 1, (1.0, 2.0)),
        ),
        (
            "press_key",
            platform.press_key(123, &parse_chord("a").unwrap()),
        ),
        ("type_text", platform.type_text(123, "hi")),
    ];
    for (action, result) in failures {
        assert_eq!(
            result.unwrap_err(),
            injection_failed(format!("{action} failed: no cg access"))
                .with_details(json!({"pid": 123}))
        );
    }
    world.desktop.state().post_error = Some(PostError("e".repeat(500)));
    assert_eq!(
        platform
            .click(123, (1.0, 2.0), MouseButton::Left, 1)
            .unwrap_err()
            .message,
        format!("click failed: {}", "e".repeat(200))
    );
}

// --- screencapture -----------------------------------------------------------------

fn shot_world() -> World {
    let world = world();
    world.script.add_file(SCREENCAPTURE);
    world
}

#[test]
fn a_window_id_captures_that_window_only() {
    let world = shot_world();
    world.script.on_png(&["-x", "-o", "-l"], (400, 300));
    let captured = world
        .platform
        .capture(CaptureRequest::Region {
            origin: (10, 20),
            size: (400, 300),
            window_id: Some(4321),
        })
        .unwrap();
    let argv = world.script.calls().remove(0);
    assert_eq!(
        argv,
        [
            SCREENCAPTURE,
            "-x",
            "-o",
            "-l",
            "4321",
            captured.path.as_str()
        ]
    );
    assert_eq!(
        Path::new(&captured.path).parent(),
        Some(world.shots.path().join("shots").as_path())
    );
    assert_eq!(
        (captured.width, captured.height, captured.logical_rect),
        (400, 300, None)
    );
}

#[test]
fn without_a_window_id_the_region_is_captured_and_size_checked() {
    let world = shot_world();
    world.script.on_png(&["-x", "-o", "-R"], (800, 600));
    let region = CaptureRequest::Region {
        origin: (10, 20),
        size: (400, 300),
        window_id: None,
    };
    world.platform.capture(region).unwrap();
    assert_eq!(
        world.script.calls()[0][..5],
        [SCREENCAPTURE, "-x", "-o", "-R", "10,20,400,300"]
    );
    let world = shot_world();
    world.script.on_png(&["-x", "-o", "-R"], (801, 600));
    assert_eq!(
        world.platform.capture(region).unwrap_err(),
        transport("captured image is 801x600, far from the requested 400x300 region")
    );
    let world = shot_world();
    world.script.on_png(&["-x", "-o", "-l"], (5000, 5000));
    let window = CaptureRequest::Region {
        origin: (0, 0),
        size: (5, 5),
        window_id: Some(1),
    };
    assert!(world.platform.capture(window).is_ok());
}

#[test]
fn the_tool_is_the_absolute_path_else_path_made_absolute_else_transport() {
    let world = world();
    world.script.on_png(&["-x"], (5, 5));
    let region = CaptureRequest::Region {
        origin: (0, 0),
        size: (5, 5),
        window_id: None,
    };
    assert_eq!(
        world.platform.capture(region).unwrap_err(),
        transport("screencapture is not available on this system")
    );
    assert!(world.script.calls().is_empty());
    let world = world_with_script(Script::with_tools(&["screencapture"]));
    world.script.on_png(&["-x"], (5, 5));
    world.platform.capture(region).unwrap();
    assert_eq!(world.script.calls()[0][0], "/usr/bin/screencapture");
    assert_eq!(
        absolute_path("rel/screencapture"),
        std::env::current_dir()
            .unwrap()
            .join("rel/screencapture")
            .to_string_lossy()
    );
}

fn world_with_script(script: Script) -> World {
    let desktop = FakeDesktop::default();
    let ax = FakeAx::default();
    let shots = tempfile::tempdir().unwrap();
    let capture = CaptureDir::new(
        shots.path().join("shots"),
        shots.path().join("unrelated-home"),
    );
    World {
        platform: MacPlatform::new(script.clone(), ax.clone(), desktop.clone(), capture),
        ax,
        desktop,
        script,
        shots,
    }
}

#[test]
fn screencapture_failures_map_to_their_codes() {
    let region = CaptureRequest::Region {
        origin: (0, 0),
        size: (5, 5),
        window_id: Some(9),
    };
    let world = shot_world();
    world
        .script
        .on(&["-x"], 1, b"", b"could not create image from window\n");
    assert_eq!(
        world.platform.capture(region).unwrap_err(),
        not_running("screencapture could not find the window: could not create image from window")
    );
    let world = shot_world();
    world.script.on(&["-x"], 2, b"", b"boom");
    assert_eq!(
        world.platform.capture(region).unwrap_err(),
        transport("screencapture failed with exit code 2: boom")
    );
    let world = shot_world();
    world.script.on_error(&["-x"], RunError::TimedOut);
    assert_eq!(
        world.platform.capture(region).unwrap_err(),
        transport("screencapture timed out after 10 seconds")
    );
    let world = shot_world();
    world.script.on(&["-x"], 0, b"", b"");
    assert_eq!(
        world.platform.capture(region).unwrap_err().code,
        ErrorCode::TransportError
    );
    let bad = CaptureRequest::Region {
        origin: (0, 0),
        size: (0, 5),
        window_id: None,
    };
    assert_eq!(
        world.platform.capture(bad).unwrap_err(),
        invalid("size must be a (width, height) pair with positive values")
            .with_details(json!({"size": [0, 5]}))
    );
}

#[test]
fn a_symlinked_capture_dir_is_refused_before_the_tool_runs() {
    let world = shot_world();
    let target = world.shots.path().join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, world.shots.path().join("shots")).unwrap();
    let region = CaptureRequest::Region {
        origin: (0, 0),
        size: (5, 5),
        window_id: None,
    };
    let error = world.platform.capture(region).unwrap_err();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(error.message.contains("symlink"), "{}", error.message);
    assert!(world.script.calls().is_empty());
}

// --- workspace ---------------------------------------------------------------------

#[test]
fn running_apps_are_the_regular_ones_with_a_bundle_id() {
    let world = world();
    let mut nameless = workspace_app("com.example.nameless", "", 3);
    nameless.name = None;
    let mut agent = workspace_app("com.example.agent", "Agent", 4);
    agent.regular = false;
    let mut anonymous = workspace_app("", "Anonymous", 5);
    anonymous.bundle_id = None;
    world.desktop.state().running = vec![
        workspace_app("com.apple.TextEdit", "TextEdit", 2),
        nameless,
        agent,
        anonymous,
    ];
    assert_eq!(
        world.platform.running_apps().unwrap(),
        [
            RunningApp {
                bundle_id: "com.apple.TextEdit".to_string(),
                name: "TextEdit".to_string(),
                pid: 2,
                path: Some("/Applications/TextEdit.app".to_string()),
            },
            RunningApp {
                bundle_id: "com.example.nameless".to_string(),
                name: "com.example.nameless".to_string(),
                pid: 3,
                path: Some("/Applications/.app".to_string()),
            },
        ]
    );
}

#[test]
fn spotlight_resolves_names_without_launching() {
    let world = world();
    world
        .desktop
        .state()
        .bundles
        .insert("/Applications/Acme.app".into(), "com.acme.app".to_string());
    world.script.on(
        &[],
        0,
        b"/Applications/Acme.app\n/Applications/Acme.app\n",
        b"",
    );
    assert_eq!(
        world
            .platform
            .bundle_for_name("Ac\"me*?\\")
            .unwrap()
            .as_deref(),
        Some("com.acme.app")
    );
    assert_eq!(
        world.script.calls(),
        [[
            "mdfind",
            "kMDItemContentTypeTree == \"com.apple.application\" && kMDItemDisplayName == \"Ac\\\"me\\*\\?\\\\\""
        ]]
    );
    assert_eq!(world.platform.bundle_for_name("  ").unwrap(), None);
    assert_eq!(world.script.calls().len(), 1);
}

#[test]
fn spotlight_failures_resolve_nothing_and_two_bundles_are_ambiguous() {
    let world = world();
    world.script.on_error(&[], RunError::TimedOut);
    assert_eq!(world.platform.bundle_for_name("Acme").unwrap(), None);
    let world = world_with_script(Script::default());
    world.script.on(&[], 1, b"/Applications/Acme.app\n", b"");
    world
        .desktop
        .state()
        .bundles
        .insert("/Applications/Acme.app".into(), "com.acme.app".to_string());
    assert_eq!(world.platform.bundle_for_name("Acme").unwrap(), None);

    let world = world_with_script(Script::default());
    let listing: String = (1..=7).fold(String::new(), |mut listing, index| {
        let _ = writeln!(listing, "/Applications/A{index}.app");
        listing
    });
    world.script.on(&[], 0, listing.as_bytes(), b"");
    {
        let mut state = world.desktop.state();
        state
            .bundles
            .insert("/Applications/A1.app".into(), "com.a.one".to_string());
        state
            .bundles
            .insert("/Applications/A6.app".into(), "com.a.six".to_string());
    }
    // The sixth hit is beyond the five-result cap.
    assert_eq!(
        world.platform.bundle_for_name("A").unwrap().as_deref(),
        Some("com.a.one")
    );
    world
        .desktop
        .state()
        .bundles
        .insert("/Applications/A3.app".into(), "com.a.three".to_string());
    assert_eq!(
        world.platform.bundle_for_name("A").unwrap_err(),
        ComputerUseError::new(
            ErrorCode::AmbiguousApp,
            "the name 'A' matches several installed apps (com.a.one, com.a.three); call get_app \
             with the bundle_id of the one you want"
        )
        .with_details(json!({"bundle_ids": ["com.a.one", "com.a.three"]}))
    );
}

#[test]
fn launch_opens_in_the_background_and_waits_for_the_app() {
    let world = world();
    world.desktop.state().launched = Some((workspace_app("com.acme.app", "Acme", 55), 2));
    let launched = world.platform.launch("com.acme.app").unwrap();
    assert_eq!(
        (launched.bundle_id.as_str(), launched.pid),
        ("com.acme.app", 55)
    );
    assert_eq!(world.script.calls(), [["open", "-g", "-b", "com.acme.app"]]);

    let world = world_with_script(Script::default())
        .platform
        .with_appear_timeout(Duration::from_millis(5), Duration::from_millis(1));
    assert_eq!(
        world.launch("com.acme.app").unwrap_err(),
        not_running(
            "com.acme.app did not start within 0 seconds; call get_app again once it is running"
        )
        .with_details(json!({"spec": "{'bundle_id': 'com.acme.app'}"}))
    );
}

#[test]
fn launch_failures_are_app_launch_failed() {
    let world = world();
    world
        .script
        .on(&[], 1, b"", b"Unable to find application\n");
    assert_eq!(
        world.platform.launch("com.acme.app").unwrap_err(),
        ComputerUseError::new(
            ErrorCode::AppLaunchFailed,
            "open failed for com.acme.app: Unable to find application"
        )
        .with_details(json!({"command": "open -g -b com.acme.app"}))
    );
    let world = world_with_script(Script::default());
    world.script.on_error(&[], RunError::TimedOut);
    assert_eq!(
        world.platform.launch("com.acme.app").unwrap_err(),
        ComputerUseError::new(
            ErrorCode::AppLaunchFailed,
            "open timed out after 10 seconds for com.acme.app"
        )
    );
    let world = world_with_script(Script::default());
    world
        .script
        .on_error(&[], RunError::Unavailable("No such file".to_string()));
    assert_eq!(
        world.platform.launch("com.acme.app").unwrap_err(),
        ComputerUseError::new(
            ErrorCode::AppLaunchFailed,
            "open is not available: No such file"
        )
    );
}

#[test]
fn bundle_paths_expand_the_home() {
    let world = world();
    let home = std::env::home_dir().unwrap();
    world
        .desktop
        .state()
        .bundles
        .insert(home.join("Apps/Acme.app"), "com.acme.app".to_string());
    assert_eq!(
        world
            .platform
            .bundle_id_for_path("~/Apps/Acme.app")
            .as_deref(),
        Some("com.acme.app")
    );
}

#[test]
fn the_lock_fails_closed_and_the_grants_map_to_states() {
    let world = world();
    let cases = [(None, true), (Some(true), true), (Some(false), false)];
    for (locked, expected) in cases {
        world.desktop.state().locked = locked;
        assert_eq!(world.platform.screen_locked(), expected);
    }
    {
        let mut state = world.desktop.state();
        state.trusted = Some(true);
        state.screen_capture = None;
    }
    assert_eq!(
        world.platform.permissions(),
        PermissionReport::Grants {
            accessibility: PermissionState::Ok,
            screen_recording: PermissionState::Unknown,
        }
    );
}

#[test]
fn activate_and_frontmost_go_through_the_workspace() {
    let world = world();
    world.desktop.state().running = vec![workspace_app("com.apple.TextEdit", "TextEdit", 2)];
    world.desktop.state().frontmost = Some(2);
    world.platform.activate(2).unwrap();
    assert_eq!(world.desktop.state().activated, [2]);
    assert!(world.platform.is_frontmost(2).unwrap());
    assert!(!world.platform.is_frontmost(3).unwrap());
    assert_eq!(
        world.platform.activate(3).unwrap_err(),
        not_running("the app is no longer running; bind it again with get_app()")
            .with_details(json!({"pid": 3}))
    );
}

// --- the App layer on macOS --------------------------------------------------------

fn app_env() -> (Env<Fake>, World) {
    let (app, _, child) = main_window();
    child.set("AXTitle", Ok(AxValue::Text("Save".to_string())));
    child.actions(Ok(vec!["AXPress".to_string()]));
    let world = world_with(FakeAx::with_app(4242, app));
    {
        let mut state = world.desktop.state();
        state.running = vec![workspace_app("com.apple.TextEdit", "TextEdit", 4242)];
        state.locked = Some(false);
        state.trusted = Some(true);
        state.screen_capture = Some(true);
    }
    let platform = MacPlatform::new(
        world.script.clone(),
        world.ax.clone(),
        world.desktop.clone(),
        CaptureDir::new(
            world.shots.path().join("shots"),
            world.shots.path().join("home"),
        ),
    );
    (Env::with_platform(platform, &["com.apple.TextEdit"]), world)
}

#[test]
fn get_app_binds_observes_and_presses_an_element() {
    let (env, world) = app_env();
    let app = env
        .session
        .get_app(&AppSpec::text("TextEdit"), None)
        .unwrap();
    assert_eq!(
        (app.bundle_id.as_str(), app.pid),
        ("com.apple.TextEdit", 4242)
    );
    let state = app.state.clone().unwrap();
    assert!(state.contains("'Main'"), "{state}");
    assert!(state.contains("Save"), "{state}");
    env.session
        .call(
            app.handle,
            AppCall::Click {
                target: TargetArg::Index(0),
                button: MouseButton::Left,
                count: 1,
            },
        )
        .unwrap();
    assert!(world
        .ax
        .calls()
        .contains(&AxCall::Perform(3, "AXPress".to_string())));
    assert!(world.desktop.state().posted.is_empty());
}

#[test]
fn a_revoked_accessibility_grant_stops_the_app() {
    let (env, world) = app_env();
    let app = env
        .session
        .get_app(&AppSpec::text("TextEdit"), None)
        .unwrap();
    world.desktop.state().trusted = Some(false);
    let error = env.session.call(app.handle, AppCall::Activate).unwrap_err();
    assert_eq!(error.code, ErrorCode::PermissionsNotGranted);
    world.desktop.state().trusted = Some(true);
    world.desktop.state().locked = None;
    let error = env.session.call(app.handle, AppCall::Activate).unwrap_err();
    assert_eq!(error.code, ErrorCode::ScreenLocked);
}

#[test]
fn the_messaging_timeout_is_one_and_a_half_seconds() {
    assert_eq!(MESSAGING_TIMEOUT, Duration::from_millis(1500));
}
