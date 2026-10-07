//! Ported from the skill's `tests/test_linux.py` (and its `fakes_linux`
//! fixtures): tree parsing, observation, discovery, input argv, the error
//! taxonomy, fingerprints, capture, and the App layer end to end on a
//! scripted X11 server.

use serde_json::json;

use super::tree::{parse_tree, Window};
use super::*;
use crate::error::ErrorCode;
use crate::keymap::parse_chord;
use crate::process::script::Script;
use crate::process::RunError;
use crate::render::serialize;
use crate::session::fake::Env;
use crate::session::{AppCall, IndexArg, PointArg, TargetArg, TextArg};
use crate::telemetry::Outcome;

/// The golden `xwininfo -root -tree -int` output (the upstream xwininfo.c
/// printf shapes: child-count lines, raw unescaped names, the geometry pair).
const ROOT_TREE: &str = concat!(
    "\n",
    "xwininfo: Window id: 63 (the root window) (has no name)\n",
    "\n",
    "  Root window id: 63 (the root window) (has no name)\n",
    "  Parent window id: 0 (none)\n",
    "     4 children:\n",
    "     104 \"Notes: draft (v2)\": (\"notes\" \"Notes\")  800x600+100+80  +100+80\n",
    "        1 children:\n",
    "        105 (has no name): (\"notes\" \"Notes\")  700x500+10+30  +110+110\n",
    "     220 \"Slack - engineering\": (\"slack\" \"Slack\")  1024x768+1920+0  +1920+0\n",
    "        1 children:\n",
    "        221 (has no name): (\"slack\" \"Slack\")  1000x700+12+40  +1932+40\n",
    "           1 children:\n",
    "           222 \"terminal\": (\"xterm\" \"XTerm\")  640x480-10-20  +1922+20\n",
    "     230 (has no name): ()  1x1+0+0  +0+0\n",
    "     240 \"Ends: (\"\": (\"weird\" \"Weird\")  300x200+5+5  +5+5\n",
    "     250 (has no name): (\"sh\" \"Sh\")",
);

fn window_line(
    window_id: i64,
    depth: usize,
    title: Option<&str>,
    class: Option<(&str, &str)>,
) -> String {
    let indent = " ".repeat(5 + 3 * depth);
    let name = title.map_or_else(
        || " (has no name)".to_string(),
        |title| format!(" \"{title}\""),
    );
    let class = class.map_or_else(
        || "()".to_string(),
        |(instance, class)| format!("(\"{instance}\" \"{class}\")"),
    );
    format!("{indent}{window_id}{name}: {class}   10x10+0+0  +0+0")
}

fn chain_tree(root: i64, length: usize) -> String {
    let mut lines = vec![window_line(root, 0, None, None)];
    for index in 1..=length {
        lines.push(window_line(
            root + i64::try_from(index).unwrap(),
            index,
            Some(&format!("node {index}")),
            None,
        ));
    }
    lines.join("\n")
}

fn flat_tree(root: i64, children: i64) -> String {
    let mut lines = vec![window_line(root, 0, None, None)];
    lines.extend((0..children).map(|index| window_line(root + 1000 + index, 1, None, None)));
    lines.join("\n")
}

#[allow(clippy::too_many_arguments)] // the parsed tuple, as the Python fixture spells it
fn node(
    window_id: i64,
    depth: usize,
    title: Option<&str>,
    instance: Option<&str>,
    wm_class: Option<&str>,
    geometry: Option<(i64, i64, i64, i64)>,
    absolute: Option<(i64, i64)>,
) -> Window {
    Window {
        id: window_id,
        depth,
        title: title.map(ToString::to_string),
        instance: instance.map(ToString::to_string),
        wm_class: wm_class.map(ToString::to_string),
        width: geometry.map(|g| g.0),
        height: geometry.map(|g| g.1),
        rel_x: geometry.map(|g| g.2),
        rel_y: geometry.map(|g| g.3),
        abs_x: absolute.map(|a| a.0),
        abs_y: absolute.map(|a| a.1),
    }
}

/// A scripted X11 session: DISPLAY set, the tools on PATH, the golden tree served.
fn x11(tools: &[&str]) -> Script {
    let script = Script::with_tools(tools);
    script.set_env("DISPLAY", ":42");
    script
}

const TOOLS: [&str; 4] = ["xdotool", "xwininfo", "maim", "scrot"];

fn platform(script: &Script, shots: &std::path::Path) -> X11Platform<Script> {
    X11Platform::new(
        script.clone(),
        CaptureDir::new(shots.to_path_buf(), shots.join("unrelated-home")),
    )
}

fn plain(script: &Script) -> (X11Platform<Script>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (platform(script, &tmp.path().join("shots")), tmp)
}

#[test]
fn the_golden_tree_parses_exactly() {
    assert_eq!(
        parse_tree(ROOT_TREE),
        [
            node(
                104,
                0,
                Some("Notes: draft (v2)"),
                Some("notes"),
                Some("Notes"),
                Some((800, 600, 100, 80)),
                Some((100, 80))
            ),
            node(
                105,
                1,
                None,
                Some("notes"),
                Some("Notes"),
                Some((700, 500, 10, 30)),
                Some((110, 110))
            ),
            node(
                220,
                0,
                Some("Slack - engineering"),
                Some("slack"),
                Some("Slack"),
                Some((1024, 768, 1920, 0)),
                Some((1920, 0))
            ),
            node(
                221,
                1,
                None,
                Some("slack"),
                Some("Slack"),
                Some((1000, 700, 12, 40)),
                Some((1932, 40))
            ),
            node(
                222,
                2,
                Some("terminal"),
                Some("xterm"),
                Some("XTerm"),
                Some((640, 480, -10, -20)),
                Some((1922, 20))
            ),
            node(230, 0, None, None, None, Some((1, 1, 0, 0)), Some((0, 0))),
            node(
                240,
                0,
                Some("Ends: (\""),
                Some("weird"),
                Some("Weird"),
                Some((300, 200, 5, 5)),
                Some((5, 5))
            ),
            node(250, 0, None, Some("sh"), Some("Sh"), None, None),
        ]
    );
}

#[test]
fn an_unsupported_encoding_name_keeps_the_window() {
    let line = window_line(330, 0, None, Some(("enc", "Enc"))).replace(
        " (has no name):",
        " (name in unsupported encoding ATOM 0x12):",
    );
    assert_eq!(
        parse_tree(&line),
        [node(
            330,
            0,
            None,
            Some("enc"),
            Some("Enc"),
            Some((10, 10, 0, 0)),
            Some((0, 0))
        )]
    );
}

#[test]
fn the_generated_tree_matches_the_golden_shape() {
    let windows = parse_tree(&chain_tree(900, 2));
    let depths: Vec<_> = windows.iter().map(|window| window.depth).collect();
    let titles: Vec<_> = windows
        .iter()
        .map(|window| window.title.as_deref())
        .collect();
    assert_eq!(depths, [0, 1, 2]);
    assert_eq!(titles, [None, Some("node 1"), Some("node 2")]);
}

fn served(tree: &str) -> Script {
    let script = x11(&TOOLS);
    script.serve_first(&["-root", "-tree", "-int"], tree);
    script
}

#[test]
fn observe_builds_the_contract_snapshot_from_the_subtree() {
    let script = served(ROOT_TREE);
    let (platform, _tmp) = plain(&script);
    let observation = platform.observe(220).unwrap();
    let xterm = Element {
        role: Some("window".to_string()),
        subrole: Some("XTerm".to_string()),
        title: Some("terminal".to_string()),
        position: Some((1922.0, 20.0)),
        size: Some((640.0, 480.0)),
        ..Element::default()
    };
    assert_eq!(
        observation,
        Observation {
            window_title: Some("Slack - engineering".to_string()),
            tree: vec![Element {
                role: Some("window".to_string()),
                subrole: Some("Slack".to_string()),
                position: Some((1932.0, 40.0)),
                size: Some((1000.0, 700.0)),
                children: vec![xterm],
                ..Element::default()
            }],
            refs: vec![221, 222],
            window_rect: Some(Rect::new(1920.0, 0.0, 1024.0, 768.0)),
            focused_index: None,
            window_id: Some(220),
            truncated: false,
        }
    );
    assert_eq!(
        serialize(&observation.tree),
        [
            "[0] window (Slack) @ (1932, 40) 1000x700",
            "  [1] window (XTerm) 'terminal' @ (1922, 20) 640x480"
        ]
    );
    assert_eq!(
        script.calls(),
        [["/usr/bin/xwininfo", "-root", "-tree", "-int"]]
    );
}

#[test]
fn observe_refuses_a_missing_window_and_the_root() {
    let script = served(ROOT_TREE);
    let (platform, _tmp) = plain(&script);
    for missing in [99_999, 63] {
        let error = platform.observe(missing).unwrap_err();
        assert_eq!(error.code, ErrorCode::AppNotRunning);
        assert_eq!(error.details, Some(json!({"window_id": missing})));
    }
}

#[test]
fn observe_caps_depth_and_elements_like_the_mac_walk() {
    let script = served(&chain_tree(900, 20));
    let (platform, _tmp) = plain(&script);
    let refs = platform.observe(900).unwrap().refs;
    assert_eq!(refs, (901..=912).collect::<Vec<_>>());
    let script = served(&flat_tree(800, 1600));
    let (platform, _tmp) = plain(&script);
    assert_eq!(platform.observe(800).unwrap().refs.len(), MAX_ELEMENTS);
}

#[test]
fn an_xwininfo_failure_is_a_transport_error() {
    let script = x11(&TOOLS);
    script.on(
        &["-root", "-tree", "-int"],
        1,
        b"",
        b"xwininfo: Can't open display :42\n",
    );
    let (platform, _tmp) = plain(&script);
    let error = platform.observe(104).unwrap_err();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert_eq!(
        error.message,
        "xwininfo failed with exit code 1: xwininfo: Can't open display :42"
    );
}

#[test]
fn list_apps_dedups_classes_in_first_seen_order() {
    let script = served(ROOT_TREE);
    let (platform, _tmp) = plain(&script);
    let ids: Vec<_> = platform
        .list_apps()
        .unwrap()
        .into_iter()
        .map(|app| app.id)
        .collect();
    assert_eq!(ids, ["Notes", "Slack", "XTerm", "Weird", "Sh"]);
}

#[test]
fn resolve_matches_the_wm_class_casefolded() {
    let script = served(ROOT_TREE);
    let (platform, _tmp) = plain(&script);
    let windows = |spec: &AppSpec| -> Vec<i64> {
        platform
            .resolve(spec)
            .unwrap()
            .into_iter()
            .map(|candidate| candidate.window_id)
            .collect()
    };
    assert_eq!(windows(&AppSpec::text("nOtEs")), [104, 105]);
    assert_eq!(windows(&AppSpec::dict("bundle_id", "Slack"))[0], 220);
    assert!(windows(&AppSpec::text("firefox")).is_empty());
    for bad in [
        AppSpec::text(""),
        AppSpec::text("   "),
        AppSpec::dict("path", "/usr/bin/firefox"),
        AppSpec::dict("weird", "x"),
    ] {
        assert_eq!(
            platform.resolve(&bad).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}

#[test]
fn clicks_move_then_click_with_rounded_ints_repeats_and_buttons() {
    let script = x11(&TOOLS);
    let (platform, _tmp) = plain(&script);
    platform
        .click(220, (10.6, 20.4), MouseButton::Left, 1)
        .unwrap();
    platform
        .click(220, (1.0, 2.0), MouseButton::Left, 3)
        .unwrap();
    platform
        .click(220, (1.0, 2.0), MouseButton::Middle, 1)
        .unwrap();
    platform
        .click(220, (1.0, 2.0), MouseButton::Right, 2)
        .unwrap();
    platform
        .click(220, (-0.4, 2.5), MouseButton::Left, 1)
        .unwrap();
    assert_eq!(
        script.calls(),
        [
            argv(&[
                "/usr/bin/xdotool",
                "mousemove",
                "--window",
                "220",
                "11",
                "20"
            ]),
            argv(&["/usr/bin/xdotool", "click", "1"]),
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "220", "1", "2"]),
            argv(&["/usr/bin/xdotool", "click", "--repeat", "3", "1"]),
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "220", "1", "2"]),
            argv(&["/usr/bin/xdotool", "click", "2"]),
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "220", "1", "2"]),
            argv(&["/usr/bin/xdotool", "click", "--repeat", "2", "3"]),
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "220", "0", "2"]),
            argv(&["/usr/bin/xdotool", "click", "1"]),
        ]
    );
}

#[test]
fn drag_presses_moves_and_releases() {
    let script = x11(&TOOLS);
    let (platform, _tmp) = plain(&script);
    platform.drag(220, (1.0, 2.0), (30.6, 40.7)).unwrap();
    assert_eq!(
        script.calls(),
        [
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "220", "1", "2"]),
            argv(&["/usr/bin/xdotool", "mousedown", "1"]),
            argv(&[
                "/usr/bin/xdotool",
                "mousemove",
                "--window",
                "220",
                "31",
                "41"
            ]),
            argv(&["/usr/bin/xdotool", "mouseup", "1"]),
        ]
    );
}

#[test]
fn scroll_moves_then_repeats_wheel_clicks_per_direction() {
    let script = x11(&TOOLS);
    let (platform, _tmp) = plain(&script);
    platform
        .scroll(220, ScrollDirection::Down, 2, (50.0, 60.0))
        .unwrap();
    for direction in [
        ScrollDirection::Up,
        ScrollDirection::Left,
        ScrollDirection::Right,
    ] {
        platform.scroll(220, direction, 1, (0.0, 0.0)).unwrap();
    }
    let calls = script.calls();
    assert_eq!(
        calls[..2],
        [
            argv(&[
                "/usr/bin/xdotool",
                "mousemove",
                "--window",
                "220",
                "50",
                "60"
            ]),
            argv(&[
                "/usr/bin/xdotool",
                "click",
                "--repeat",
                "20",
                "--delay",
                "50",
                "5"
            ]),
        ]
    );
    let buttons: Vec<&str> = calls
        .iter()
        .filter(|argv| argv[1] == "click")
        .map(|argv| argv.last().unwrap().as_str())
        .collect();
    assert_eq!(buttons, ["5", "4", "6", "7"]);
}

#[test]
fn chords_translate_to_x11_keysyms() {
    let cases = [
        ("cmd+shift+f", "shift+super+f"),
        ("ctrl+alt+t", "alt+ctrl+t"),
        ("Return", "Return"),
        ("enter", "Return"),
        ("space", "space"),
        ("Delete", "BackSpace"),
        ("ctrl+delete", "ctrl+BackSpace"),
        ("ForwardDelete", "Delete"),
        ("PageUp", "Prior"),
        ("shift+pagedown", "shift+Next"),
        ("alt+F5", "alt+F5"),
        ("super+c", "super+c"),
    ];
    for (chord, keysym) in cases {
        let script = x11(&TOOLS);
        let (platform, _tmp) = plain(&script);
        platform
            .press_key(220, &parse_chord(chord).unwrap())
            .unwrap();
        assert_eq!(
            script.calls(),
            [argv(&[
                "/usr/bin/xdotool",
                "key",
                "--window",
                "220",
                keysym
            ])],
            "{chord}"
        );
    }
}

#[test]
fn typing_uses_the_window_and_the_delay() {
    let script = x11(&TOOLS);
    let (platform, _tmp) = plain(&script);
    platform.type_text(220, "h\u{e9}llo").unwrap();
    assert_eq!(
        script.calls(),
        [argv(&[
            "/usr/bin/xdotool",
            "type",
            "--window",
            "220",
            "--delay",
            "12",
            "h\u{e9}llo"
        ])]
    );
}

#[test]
fn a_failed_input_run_is_injection_failed_with_capped_stderr() {
    let script = x11(&TOOLS);
    script.on(&["key"], 1, b"", &[b'x'; 500]);
    let (platform, _tmp) = plain(&script);
    let error = platform
        .press_key(220, &parse_chord("Return").unwrap())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InjectionFailed);
    assert_eq!(
        error.message,
        format!("key failed with exit code 1: {}", "x".repeat(200))
    );
    assert_eq!(
        error.details,
        Some(json!({"argv": "/usr/bin/xdotool key --window 220 Return"}))
    );
}

#[test]
fn a_missing_display_is_a_transport_error_naming_it() {
    let script = Script::with_tools(&TOOLS);
    let (platform, _tmp) = plain(&script);
    let errors = [
        platform
            .click(220, (1.0, 2.0), MouseButton::Left, 1)
            .unwrap_err(),
        platform.observe(220).unwrap_err(),
        platform.capture(CaptureRequest::Window(220)).unwrap_err(),
        platform.list_apps().unwrap_err(),
    ];
    for error in errors {
        assert_eq!(error.code, ErrorCode::TransportError);
        assert!(error.message.contains("DISPLAY"), "{}", error.message);
    }
}

#[test]
fn a_missing_tool_is_a_transport_error_naming_it() {
    let script = x11(&["xwininfo", "maim", "scrot"]);
    let (platform, _tmp) = plain(&script);
    let error = platform
        .click(220, (1.0, 2.0), MouseButton::Left, 1)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert_eq!(error.details, Some(json!({"tool": "xdotool"})));
}

#[test]
fn an_absolute_tool_candidate_wins_over_path() {
    let script = x11(&[]);
    script.add_file("/usr/bin/xdotool");
    let (platform, _tmp) = plain(&script);
    platform.type_text(1, "x").unwrap();
    assert_eq!(script.calls()[0][0], "/usr/bin/xdotool");
}

#[test]
fn timeouts_and_unrunnable_tools_are_transport_errors() {
    for error in [
        RunError::TimedOut,
        RunError::Unavailable("No such file or directory".to_string()),
    ] {
        let script = x11(&TOOLS);
        script.on_error(&["mousemove"], error);
        let (platform, _tmp) = plain(&script);
        let failed = platform
            .click(220, (1.0, 2.0), MouseButton::Left, 1)
            .unwrap_err();
        assert_eq!(failed.code, ErrorCode::TransportError);
    }
}

#[test]
fn the_live_fingerprint_reads_the_title() {
    let script = x11(&TOOLS);
    script.on(&["getwindowname"], 0, "Main \u{2014} Doc\n".as_bytes(), b"");
    let (platform, _tmp) = plain(&script);
    assert_eq!(
        platform.live_fingerprint(&220).unwrap(),
        (
            Some("window".to_string()),
            Some("Main \u{2014} Doc".to_string())
        )
    );
    assert_eq!(
        script.calls(),
        [argv(&["/usr/bin/xdotool", "getwindowname", "220"])]
    );
    for (code, stdout) in [(1, &b""[..]), (0, &b"  \n"[..])] {
        let script = x11(&TOOLS);
        script.on(&["getwindowname"], code, stdout, b"xdo error\n");
        let (platform, _tmp) = plain(&script);
        assert_eq!(
            platform.live_fingerprint(&220).unwrap(),
            (Some("window".to_string()), None)
        );
    }
}

#[test]
fn the_window_fingerprint_reads_the_subtree_and_the_input_focus() {
    let script = served(ROOT_TREE);
    script.on(&["getwindowfocus", "-f"], 0, b"221\n", b"");
    let (platform, _tmp) = plain(&script);
    let first = platform.window_fingerprint(220, Duration::ZERO).unwrap();
    assert_eq!(first[1].as_deref(), Some("221"));
    assert_eq!(platform.window_fingerprint(99, Duration::ZERO), None);
}

#[test]
fn maim_captures_the_bound_window_into_the_hardened_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let shots = tmp.path().join("shots");
    let script = x11(&TOOLS);
    script.on_png(&["-i"], (640, 480));
    let platform = platform(&script, &shots);
    let captured = platform.capture(CaptureRequest::Window(220)).unwrap();
    let calls = script.calls();
    assert_eq!(calls[0][..3], argv(&["/usr/bin/maim", "-i", "220"]));
    assert_eq!(
        std::path::Path::new(&calls[0][3]).parent(),
        Some(shots.as_path())
    );
    assert_eq!(
        captured,
        Captured {
            path: calls[0][3].clone(),
            width: 640,
            height: 480,
            logical_rect: None
        }
    );
}

#[test]
fn a_failing_maim_falls_back_to_scrot() {
    let tmp = tempfile::tempdir().unwrap();
    let script = x11(&TOOLS);
    script.on(&["-i"], 1, b"", b"maim: failed to take screenshot");
    script.on_png(&["-u"], (400, 300));
    let captured = platform(&script, &tmp.path().join("shots"))
        .capture(CaptureRequest::Window(220))
        .unwrap();
    let calls = script.calls();
    assert_eq!(calls[0][..2], argv(&["/usr/bin/maim", "-i"]));
    assert_eq!(calls[1][..3], argv(&["/usr/bin/scrot", "-u", "-o"]));
    assert_eq!((captured.width, captured.height), (400, 300));
}

#[test]
fn without_maim_scrot_runs_directly() {
    let tmp = tempfile::tempdir().unwrap();
    let script = x11(&["xdotool", "xwininfo", "scrot"]);
    script.on_png(&["-u"], (100, 100));
    let captured = platform(&script, &tmp.path().join("shots"))
        .capture(CaptureRequest::Window(220))
        .unwrap();
    assert_eq!(script.calls().len(), 1);
    assert_eq!(captured.width, 100);
}

#[test]
fn capture_failures_name_every_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let script = x11(&["xdotool", "xwininfo"]);
    let error = platform(&script, &tmp.path().join("shots"))
        .capture(CaptureRequest::Window(220))
        .unwrap_err();
    assert!(
        error.message.contains("maim") && error.message.contains("scrot"),
        "{}",
        error.message
    );
    let script = x11(&TOOLS);
    script.on(&["-i"], 1, b"", b"maim: window gone");
    script.on(&["-u"], 1, b"", b"giblib error: cannot open X display");
    let error = platform(&script, &tmp.path().join("shots"))
        .capture(CaptureRequest::Window(220))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(
        error.message.contains("maim failed with exit code 1"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("scrot failed with exit code 1"),
        "{}",
        error.message
    );
    // maim "succeeds" without writing a PNG and there is no scrot.
    let script = x11(&["xdotool", "xwininfo", "maim"]);
    script.on(&["-i"], 0, b"", b"");
    let error = platform(&script, &tmp.path().join("shots"))
        .capture(CaptureRequest::Window(220))
        .unwrap_err();
    assert!(
        error.message.starts_with(
            "window screenshot failed: maim: screencapture did not write a readable PNG"
        ),
        "{}",
        error.message
    );
}

// --- the App layer on X11 -----------------------------------------------------

fn session_env(
    tree: &str,
    allowed: &[&str],
    blocked: &[&str],
) -> (Env<X11Platform<Script>>, Script) {
    let script = x11(&["xdotool", "xwininfo", "maim", "scrot", "loginctl"]);
    script.on(&["show-session"], 0, b"LockedHint=no\nActive=yes\n", b"");
    script.serve_first(&["-root", "-tree", "-int"], tree);
    let shots = tempfile::tempdir().unwrap().keep().join("shots");
    let env = Env::with_policy(platform(&script, &shots), allowed, blocked);
    (env, script)
}

fn linux_env() -> (Env<X11Platform<Script>>, Script) {
    session_env(ROOT_TREE, &["Notes", "Slack"], &[])
}

/// The xdotool commands an action dispatched, without the settle's focus reads.
fn dispatched(script: &Script) -> Vec<Vec<String>> {
    script
        .tool_calls("xdotool")
        .into_iter()
        .filter(|argv| argv[1] != "getwindowfocus")
        .collect()
}

fn bind_notes(env: &Env<X11Platform<Script>>) -> crate::session::BoundApp {
    env.session.get_app(&AppSpec::text("notes"), None).unwrap()
}

fn point(x: f64, y: f64) -> TargetArg {
    TargetArg::Point(PointArg::Valid {
        x,
        y,
        repr: format!("({x:?}, {y:?})"),
    })
}

#[test]
fn get_app_binds_the_topmost_window_and_observes() {
    let (env, _script) = linux_env();
    let app = bind_notes(&env);
    assert_eq!(
        (app.bundle_id.as_str(), app.name.as_str(), app.pid),
        ("Notes", "Notes", 104)
    );
    let state = app.state.clone().unwrap();
    assert!(state.contains("window 'Notes: draft (v2)'"), "{state}");
    assert!(
        state.contains("[0] window (Notes) @ (110, 110) 700x500"),
        "{state}"
    );
    assert_eq!(bind_notes(&env), app);
    let slack = env.session.get_app(&AppSpec::text("slack"), None).unwrap();
    assert_eq!((slack.pid, slack.bundle_id.as_str()), (220, "Slack"));
}

#[test]
fn get_app_gates_the_wm_class_and_reports_missing_apps() {
    let (env, _script) = session_env(ROOT_TREE, &[], &[]);
    assert_eq!(
        env.session
            .get_app(&AppSpec::text("notes"), None)
            .unwrap_err()
            .code,
        ErrorCode::AppNotAllowed
    );
    let (env, _script) = session_env(ROOT_TREE, &["Notes"], &["Notes"]);
    let blocked = env
        .session
        .get_app(&AppSpec::text("notes"), None)
        .unwrap_err();
    assert!(blocked.message.contains("blocked list"));
    let (env, _script) = linux_env();
    let missing = env
        .session
        .get_app(&AppSpec::text("firefox"), None)
        .unwrap_err();
    assert_eq!(
        missing,
        crate::error::not_running(
            "'firefox' has no window on the linux desktop; start the app yourself and call get_app again"
        )
        .with_details(json!({"spec": "firefox"}))
    );
    let path = env
        .session
        .get_app(&AppSpec::dict("path", "/usr/bin/firefox"), None)
        .unwrap_err();
    assert_eq!(path.code, ErrorCode::InvalidArgument);
}

#[test]
fn clicks_on_elements_translate_to_window_relative_centers() {
    let (env, script) = linux_env();
    let app = bind_notes(&env);
    let click = AppCall::Click {
        target: TargetArg::Index(0),
        button: MouseButton::Left,
        count: 1,
    };
    env.session.call(app.handle, click).unwrap();
    assert_eq!(
        dispatched(&script),
        [
            argv(&["/usr/bin/xdotool", "getwindowname", "105"]),
            argv(&[
                "/usr/bin/xdotool",
                "mousemove",
                "--window",
                "104",
                "360",
                "280"
            ]),
            argv(&["/usr/bin/xdotool", "click", "1"]),
        ]
    );
    assert!(
        !script
            .tool_calls("xdotool")
            .iter()
            .all(|argv| argv[1] != "getwindowfocus"),
        "the settle reads the focus"
    );
}

#[test]
fn point_clicks_stay_window_relative_and_are_bounds_checked() {
    let (env, script) = linux_env();
    let app = bind_notes(&env);
    let click = AppCall::Click {
        target: point(10.0, 20.0),
        button: MouseButton::Right,
        count: 2,
    };
    env.session.call(app.handle, click).unwrap();
    assert_eq!(
        dispatched(&script),
        [
            argv(&[
                "/usr/bin/xdotool",
                "mousemove",
                "--window",
                "104",
                "10",
                "20"
            ]),
            argv(&["/usr/bin/xdotool", "click", "--repeat", "2", "3"]),
        ]
    );
    let outside = AppCall::Click {
        target: point(5000.0, 5.0),
        button: MouseButton::Left,
        count: 1,
    };
    let error = env.session.call(app.handle, outside).unwrap_err();
    assert!(
        error.message.contains("outside the observed window"),
        "{}",
        error.message
    );
}

#[test]
fn drag_scroll_keys_and_type_dispatch_to_x11() {
    let (env, script) = linux_env();
    let app = bind_notes(&env);
    let calls = [
        AppCall::Drag {
            from: PointArg::Valid {
                x: 1.0,
                y: 2.0,
                repr: "(1, 2)".to_string(),
            },
            to: PointArg::Valid {
                x: 3.0,
                y: 4.0,
                repr: "(3, 4)".to_string(),
            },
        },
        AppCall::Scroll {
            target: TargetArg::Index(0),
            direction: ScrollDirection::Down,
            pages: 2,
        },
        AppCall::PressKey {
            key: TextArg::Text("cmd+s".to_string()),
        },
        AppCall::TypeText {
            text: TextArg::Text("hello there".to_string()),
        },
    ];
    for call in calls {
        env.session.call(app.handle, call).unwrap();
    }
    assert_eq!(
        dispatched(&script),
        [
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "104", "1", "2"]),
            argv(&["/usr/bin/xdotool", "mousedown", "1"]),
            argv(&["/usr/bin/xdotool", "mousemove", "--window", "104", "3", "4"]),
            argv(&["/usr/bin/xdotool", "mouseup", "1"]),
            argv(&["/usr/bin/xdotool", "getwindowname", "105"]),
            argv(&[
                "/usr/bin/xdotool",
                "mousemove",
                "--window",
                "104",
                "360",
                "280"
            ]),
            argv(&[
                "/usr/bin/xdotool",
                "click",
                "--repeat",
                "20",
                "--delay",
                "50",
                "5"
            ]),
            argv(&["/usr/bin/xdotool", "key", "--window", "104", "super+s"]),
            argv(&[
                "/usr/bin/xdotool",
                "type",
                "--window",
                "104",
                "--delay",
                "12",
                "hello there"
            ]),
        ]
    );
}

#[test]
fn a_renamed_element_is_stale_and_a_gone_window_fails_the_guard() {
    let (env, script) = linux_env();
    let app = bind_notes(&env);
    script.on(&["getwindowname"], 0, b"renamed\n", b"");
    let click = AppCall::Click {
        target: TargetArg::Index(0),
        button: MouseButton::Left,
        count: 1,
    };
    assert_eq!(
        env.session.call(app.handle, click).unwrap_err().code,
        ErrorCode::ElementStale
    );
    script.serve_first(
        &["-root", "-tree", "-int"],
        &window_line(
            220,
            0,
            Some("Slack - engineering"),
            Some(("slack", "Slack")),
        ),
    );
    let click = AppCall::Click {
        target: point(1.0, 1.0),
        button: MouseButton::Left,
        count: 1,
    };
    assert_eq!(
        env.session.call(app.handle, click).unwrap_err().code,
        ErrorCode::AppNotRunning
    );
}

#[test]
fn the_screenshot_dispatches_to_maim_without_scaling_state() {
    let (env, script) = linux_env();
    script.on_png(&["-i"], (800, 600));
    let app = bind_notes(&env);
    let shot = env
        .session
        .call(app.handle, AppCall::GetScreenshot)
        .unwrap();
    assert_eq!(
        script.tool_calls("maim"),
        [argv(&[
            "/usr/bin/maim",
            "-i",
            "104",
            shot["path"].as_str().unwrap()
        ])]
    );
    assert_eq!(
        (shot["width"].clone(), shot["height"].clone()),
        (json!(800), json!(600))
    );
}

#[test]
fn unsupported_actions_name_the_x11_gap_and_emit_their_events() {
    let (env, _script) = linux_env();
    let app = bind_notes(&env);
    let calls = [
        (
            "paste",
            AppCall::Paste {
                text: "secret".to_string(),
                format: crate::platform::PasteFormat::Text,
            },
        ),
        (
            "set_value",
            AppCall::SetValue {
                index: IndexArg::Valid(0),
                value: "value".to_string(),
            },
        ),
        (
            "select_text",
            AppCall::SelectText {
                index: IndexArg::Valid(0),
                text: "value".to_string(),
                prefix: None,
                suffix: None,
            },
        ),
        (
            "secondary",
            AppCall::SecondaryAction {
                index: IndexArg::Valid(0),
                action: crate::session::ActionArg::Name("AXPress".to_string()),
            },
        ),
    ];
    for (action, call) in calls {
        let error = env.session.call(app.handle, call).unwrap_err();
        assert_eq!(error.code, ErrorCode::ActionUnsupported);
        assert!(
            error
                .message
                .contains("not available on the Linux X11 backend"),
            "{}",
            error.message
        );
        assert_eq!(
            env.actions().last(),
            Some(&(action, Outcome::Error(ErrorCode::ActionUnsupported)))
        );
    }
    let activate = env.session.call(app.handle, AppCall::Activate).unwrap_err();
    assert_eq!(
        activate,
        crate::error::unsupported(
            "focus control is not available on the linux X11 backend yet; keyboard flows that need \
             app focus are unsupported there"
        )
        .with_details(json!({"platform": "linux"}))
    );
    assert_eq!(
        env.actions().last(),
        Some(&("activate", Outcome::Error(ErrorCode::ActionUnsupported)))
    );
    let frontmost = env
        .session
        .call(app.handle, AppCall::IsFrontmost)
        .unwrap_err();
    assert!(frontmost
        .message
        .contains("focus control is not available on the linux X11 backend yet"));
    let ocr = env
        .session
        .call(app.handle, AppCall::GetTextRegions)
        .unwrap_err();
    assert!(
        ocr.message
            .contains("not available on the Linux X11 backend yet")
            && ocr.message.contains("get_ax_state")
    );
}

#[test]
fn typing_is_never_refused_for_a_secure_focus_on_x11() {
    // X11 has no secure-input role: a documented gap, not fail-closed.
    let (env, script) = linux_env();
    let app = bind_notes(&env);
    env.session
        .call(
            app.handle,
            AppCall::TypeText {
                text: TextArg::Text("pw".to_string()),
            },
        )
        .unwrap();
    assert!(dispatched(&script).iter().any(|argv| argv[1] == "type"));
}

#[test]
fn telemetry_diffing_and_the_state_report_linux() {
    let (env, _script) = linux_env();
    let app = bind_notes(&env);
    let click = AppCall::Click {
        target: TargetArg::Index(0),
        button: MouseButton::Left,
        count: 1,
    };
    env.session.call(app.handle, click).unwrap();
    assert_eq!(env.actions(), [("click", Outcome::Ok)]);
    assert_eq!(
        env.session
            .call(app.handle, AppCall::GetAxState { diff: true })
            .unwrap(),
        json!("(no changes since the previous observation)")
    );
    let state = env.session.get_state(false).unwrap();
    assert_eq!(state["platform"], json!("linux"));
    assert_eq!(state["allowlist"]["allowed"], json!(["Notes", "Slack"]));
    assert_eq!(
        state["apps"][0],
        json!({"id": "Notes", "name": "Notes", "running": true})
    );
    let status = env.session.permissions();
    assert_eq!(
        (
            status["accessibility"].clone(),
            status["screen_recording"].clone()
        ),
        (json!("unknown"), json!("unknown"))
    );
    assert!(status["help"][0].as_str().unwrap().contains("Linux"));
}

#[test]
fn the_x11_lock_reads_logind() {
    let script = x11(&["loginctl"]);
    script.on(&["show-session"], 0, b"LockedHint=no\nActive=yes\n", b"");
    let (platform, _tmp) = plain(&script);
    assert!(!platform.screen_locked());
}
