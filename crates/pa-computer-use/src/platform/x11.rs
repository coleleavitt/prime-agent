//! The Linux X11 backend: `xwininfo` observation, `xdotool` input, `maim`
//! (or `scrot`) capture.
//!
//! X11 has no widget tree below client windows and no secure-input role:
//! observation is the bound window's subtree of X11 windows (every element
//! `window` with its `WM_CLASS` as the subrole, absolute root-window
//! positions), input is window-relative `xdotool --window` delivery (synthetic
//! keys, a pointer moved into the window then a plain `XTest` click), and
//! binding attaches to a running window by `WM_CLASS` (no launch story).
//! Paste, `set_value`, `select_text`, secondary actions and focus control have
//! no X11 backing; the session refuses them by name.
//!
//! The tools stay subprocesses (as in the Python skill): their synthetic
//! delivery semantics (`XSendEvent` with `--window`, `XTest` clicks, the
//! keysym table) are the behaviour, and a reimplementation over the X
//! protocol could not be exercised here.

mod tree;

use std::time::Duration;

use serde_json::json;

use crate::capture::CaptureDir;
use crate::element::{Element, Observation, Pair, Rect, MAX_DEPTH, MAX_ELEMENTS};
use crate::error::{
    head, injection_failed, invalid, not_running, transport, ComputerUseError, Result, ERROR_LIMIT,
};
use crate::keymap::{Modifier, ParsedChord};
use crate::permissions::PermissionReport;
use crate::platform::logind;
use crate::platform::{
    AppEntry, CaptureRequest, Captured, Discovery, Fingerprint, MouseButton, Platform,
    PlatformKind, ScrollDirection, Target, WindowCandidate, WindowDirectory,
};
use crate::process::{optional_tool, run_tool, CommandOutput, Tools, TOOL_TIMEOUT};
use crate::secure::Security;
use crate::spec::{is_blank, AppSpec, SpecKey, SpecShape};

use tree::{parse_tree, Window};

const TYPE_DELAY_MS: u32 = 12;
const WHEEL_CLICKS_PER_PAGE: u32 = 10;
const WHEEL_REPEAT_DELAY_MS: u32 = 50;

/// The absolute tool candidates, tried before `PATH`.
fn absolute(name: &str) -> Option<&'static str> {
    match name {
        "xdotool" => Some("/usr/bin/xdotool"),
        "xwininfo" => Some("/usr/bin/xwininfo"),
        "maim" => Some("/usr/bin/maim"),
        "scrot" => Some("/usr/bin/scrot"),
        "loginctl" => Some("/usr/bin/loginctl"),
        _ => None,
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(ToString::to_string).collect()
}

/// One coordinate for an xdotool argv: Python's `str(int(round(value)))`.
fn coord(value: f64) -> String {
    #[allow(clippy::cast_possible_truncation)] // screen coordinates are far inside i64
    let rounded = value.round_ties_even() as i64;
    rounded.to_string()
}

/// The X11 backend over a process environment.
pub(crate) struct X11Platform<T: Tools> {
    tools: T,
    capture: CaptureDir,
}

impl<T: Tools> X11Platform<T> {
    pub(crate) fn new(tools: T, capture: CaptureDir) -> Self {
        Self { tools, capture }
    }

    fn require_display(&self) -> Result<()> {
        if self
            .tools
            .env("DISPLAY")
            .is_none_or(|display| display.is_empty())
        {
            return Err(transport(
                "computer use backend unavailable: DISPLAY is not set; the Linux backend needs an \
                 X server",
            ));
        }
        Ok(())
    }

    fn tool(&self, name: &str) -> Result<String> {
        optional_tool(&self.tools, name, absolute(name)).ok_or_else(|| {
            transport(format!(
                "computer use backend unavailable: the Linux backend needs the {name} tool on PATH"
            ))
            .with_details(json!({"tool": name}))
        })
    }

    fn run(&self, argv: &[String]) -> Result<CommandOutput> {
        run_tool(&self.tools, argv, TOOL_TIMEOUT)
    }

    /// Run one input command; a nonzero exit is `INJECTION_FAILED` (the
    /// action did not deliver, the model may retry).
    fn run_checked(&self, argv: &[String], action: &str) -> Result<()> {
        let output = self.run(argv)?;
        if !output.success() {
            return Err(injection_failed(format!(
                "{action} failed with exit code {}: {}",
                output.code,
                output.capped()
            ))
            .with_details(json!({"argv": head(&argv.join(" "), ERROR_LIMIT)})));
        }
        Ok(())
    }

    /// The whole root window tree, depth-first.
    fn root_windows(&self) -> Result<Vec<Window>> {
        self.require_display()?;
        let output = self.run(&[
            self.tool("xwininfo")?,
            "-root".into(),
            "-tree".into(),
            "-int".into(),
        ])?;
        if !output.success() {
            return Err(transport(format!(
                "xwininfo failed with exit code {}: {}",
                output.code,
                output.capped()
            )));
        }
        Ok(parse_tree(&String::from_utf8_lossy(&output.stdout)))
    }

    /// The windows whose `WM_CLASS` matches the spec, casefolded.
    fn resolve_class(&self, spec: &AppSpec) -> Result<Vec<Window>> {
        let wanted = spec_class(spec)?;
        Ok(self
            .root_windows()?
            .into_iter()
            .filter(|window| {
                window
                    .wm_class
                    .as_deref()
                    .is_some_and(|class| crate::pyfmt::casefold_eq(class, &wanted))
            })
            .collect())
    }

    fn mousemove(&self, xdotool: &str, window_id: Target, (x, y): Pair) -> Result<()> {
        self.run_checked(
            &argv(&[
                xdotool,
                "mousemove",
                "--window",
                &window_id.to_string(),
                &coord(x),
                &coord(y),
            ]),
            "mousemove",
        )
    }

    fn screenshot(&self, window_id: Target) -> Result<Captured> {
        self.require_display()?;
        let maim = optional_tool(&self.tools, "maim", absolute("maim"));
        let scrot = optional_tool(&self.tools, "scrot", absolute("scrot"));
        if maim.is_none() && scrot.is_none() {
            return Err(transport(
                "computer use backend unavailable: the Linux backend needs maim (preferred) or \
                 scrot on PATH for screenshots",
            ));
        }
        let dir = self.capture.open()?;
        let (name, path) = dir.new_target()?;
        let path = path.to_string_lossy().into_owned();
        let mut attempts: Vec<(&str, Vec<String>)> = Vec::new();
        if let Some(maim) = &maim {
            attempts.push(("maim", argv(&[maim, "-i", &window_id.to_string(), &path])));
        }
        // scrot captures the FOCUSED window, not the bound one: the
        // documented fallback when maim is absent or fails.
        if let Some(scrot) = &scrot {
            attempts.push(("scrot", argv(&[scrot, "-u", "-o", &path])));
        }
        let mut reasons = Vec::new();
        for (tool, command) in attempts {
            let output = self.run(&command)?;
            if !output.success() {
                reasons.push(format!(
                    "{tool} failed with exit code {}: {}",
                    output.code,
                    output.capped()
                ));
                continue;
            }
            match dir.png_dimensions(&name) {
                Ok((width, height)) => {
                    dir.make_private(&name);
                    dir.sweep(Some(&name));
                    return Ok(Captured {
                        path,
                        width,
                        height,
                        logical_rect: None,
                    });
                }
                Err(error) => reasons.push(format!("{tool}: {}", error.message)),
            }
        }
        Err(transport(
            head(
                &format!("window screenshot failed: {}", reasons.join("; ")),
                ERROR_LIMIT,
            )
            .to_string(),
        ))
    }
}

/// The `WM_CLASS` one spec names (stripped), refusing launch-path shapes.
fn spec_class(spec: &AppSpec) -> Result<String> {
    match &spec.shape {
        SpecShape::Text(text) => {
            if is_blank(text) {
                return Err(
                    invalid("the app spec must not be empty").with_details(json!({"spec": ""}))
                );
            }
            return Ok(text.trim().to_string());
        }
        SpecShape::Dict { .. } => {
            for key in [SpecKey::BundleId, SpecKey::Name] {
                if let Some(value) = spec.entry(key).filter(|value| !is_blank(value)) {
                    return Ok(value.trim().to_string());
                }
            }
        }
        SpecShape::Other(_) => {}
    }
    Err(invalid(
        "the app spec must be an app name or a {\"bundle_id\"|\"name\"} dict; X11 windows have no \
         launch paths",
    )
    .with_details(json!({"spec": head(&spec.display, 64)})))
}

/// The bound window's children as elements and refs, capped like the
/// macOS walk (12 levels, 1500 elements).
fn subtree(windows: &[Window], index: usize) -> (Vec<Element>, Vec<Target>) {
    let bound = &windows[index];
    let mut tree: Vec<Element> = Vec::new();
    let mut refs = Vec::new();
    // The child index of each open ancestor, from the bound window down.
    let mut path: Vec<usize> = Vec::new();
    for window in &windows[index + 1..] {
        let Some(depth) = window
            .depth
            .checked_sub(bound.depth)
            .filter(|&depth| depth > 0)
        else {
            break;
        };
        if depth > MAX_DEPTH {
            continue;
        }
        if refs.len() >= MAX_ELEMENTS {
            break;
        }
        path.truncate(depth - 1);
        let mut siblings = &mut tree;
        for &child in &path {
            siblings = &mut siblings[child].children;
        }
        path.push(siblings.len());
        siblings.push(element(window));
        refs.push(window.id);
    }
    (tree, refs)
}

fn element(window: &Window) -> Element {
    #[allow(clippy::cast_precision_loss)] // screen coordinates are far inside f64's mantissa
    let float = |value: i64| value as f64;
    Element {
        role: Some("window".to_string()),
        subrole: window.wm_class.clone(),
        title: window.title.clone(),
        position: window
            .abs_x
            .zip(window.abs_y)
            .map(|(x, y)| (float(x), float(y))),
        size: window
            .width
            .zip(window.height)
            .map(|(w, h)| (float(w), float(h))),
        ..Element::default()
    }
}

/// One chord as an xdotool keysequence with X11 keysym names: modifiers
/// sorted (cmd is super), keys translated (Delete is `BackSpace`,
/// `ForwardDelete` is Delete, `PageUp` Prior, `PageDown` Next, Space space).
fn chord_keysym(chord: &ParsedChord) -> String {
    let mut parts: Vec<&str> = chord
        .modifiers
        .iter()
        .map(|modifier| match modifier {
            Modifier::Cmd => "super",
            Modifier::Ctrl => "ctrl",
            Modifier::Alt => "alt",
            Modifier::Shift => "shift",
        })
        .collect();
    parts.sort_unstable();
    parts.push(named_keysym(&chord.key).unwrap_or(&chord.key));
    parts.join("+")
}

/// The X11 keysym of a named key (single characters pass through).
pub(crate) fn named_keysym(key: &str) -> Option<&str> {
    Some(match key {
        "Return" => "Return",
        "Tab" => "Tab",
        "Escape" => "Escape",
        "Space" => "space",
        "Delete" => "BackSpace",
        "ForwardDelete" => "Delete",
        "Home" => "Home",
        "End" => "End",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        "Up" => "Up",
        "Down" => "Down",
        "Left" => "Left",
        "Right" => "Right",
        function
            if function.len() > 1
                && function.starts_with('F')
                && function[1..].parse::<u8>().is_ok() =>
        {
            function
        }
        _ => return None,
    })
}

impl<T: Tools> WindowDirectory for X11Platform<T> {
    /// One entry per distinct `WM_CLASS` in tree order; windows without one
    /// (window-manager frames, helpers) are not apps.
    fn list_apps(&self) -> Result<Vec<AppEntry>> {
        let mut apps: Vec<AppEntry> = Vec::new();
        for window in self.root_windows()? {
            if let Some(class) = window.wm_class {
                if !apps.iter().any(|app| app.id == class) {
                    apps.push(AppEntry {
                        id: class.clone(),
                        name: class,
                    });
                }
            }
        }
        Ok(apps)
    }

    /// Every window of the spec's `WM_CLASS`; the first in tree order (the
    /// topmost) binds.
    fn resolve(&self, spec: &AppSpec) -> Result<Vec<WindowCandidate>> {
        Ok(self
            .resolve_class(spec)?
            .into_iter()
            .filter_map(|window| {
                Some(WindowCandidate {
                    app_id: window.wm_class?,
                    window_id: window.id,
                })
            })
            .collect())
    }

    fn owns(&self, window_id: i64, app_id: &str) -> Result<bool> {
        Ok(self
            .resolve_class(&AppSpec::text(app_id))?
            .iter()
            .any(|window| window.id == window_id))
    }
}

impl<T: Tools + 'static> Platform for X11Platform<T> {
    type Element = Target;

    fn kind(&self) -> PlatformKind {
        PlatformKind::X11
    }

    fn discovery(&self) -> Discovery<'_> {
        Discovery::Windows(self)
    }

    fn screen_locked(&self) -> bool {
        logind::screen_locked(&self.tools, absolute("loginctl"))
    }

    fn permissions(&self) -> PermissionReport {
        PermissionReport::X11
    }

    /// The bound window's subtree from the root tree; its own line gives
    /// the absolute rect. X11 has no accessibility focus.
    fn observe(&self, window_id: Target) -> Result<Observation<Target>> {
        let windows = self.root_windows()?;
        let Some(index) = windows.iter().position(|window| window.id == window_id) else {
            return Err(not_running(format!(
                "window {window_id} is not in the window tree; call get_app again to re-bind it"
            ))
            .with_details(json!({"window_id": window_id})));
        };
        let bound = &windows[index];
        #[allow(clippy::cast_precision_loss)] // screen coordinates are far inside f64's mantissa
        let window_rect = match (bound.abs_x, bound.abs_y, bound.width, bound.height) {
            (Some(x), Some(y), Some(width), Some(height)) => {
                Some(Rect::new(x as f64, y as f64, width as f64, height as f64))
            }
            _ => None,
        };
        let (tree, refs) = subtree(&windows, index);
        Ok(Observation {
            window_title: bound.title.clone(),
            tree,
            refs,
            window_rect,
            focused_index: None,
            window_id: Some(window_id),
            truncated: false,
        })
    }

    /// The live (role, title): the constant `window` and `xdotool
    /// getwindowname`; an unreadable title reads as `None`.
    fn live_fingerprint(&self, window_id: &Target) -> Result<(Option<String>, Option<String>)> {
        self.require_display()?;
        let output = self.run(&argv(&[
            &self.tool("xdotool")?,
            "getwindowname",
            &window_id.to_string(),
        ]))?;
        let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let title = (output.success() && !title.is_empty()).then_some(title);
        Ok((Some("window".to_string()), title))
    }

    /// The bound subtree plus the X input focus window (which moves onto a
    /// popup that grabs focus without changing the subtree). In-widget
    /// edits do not change it: the read settles at once there.
    fn window_fingerprint(&self, window_id: Target, _budget: Duration) -> Option<Fingerprint> {
        let windows = self.root_windows().ok()?;
        let index = windows.iter().position(|window| window.id == window_id)?;
        let (tree, _) = subtree(&windows, index);
        let xdotool = self.tool("xdotool").ok()?;
        let focus = self.run(&argv(&[&xdotool, "getwindowfocus", "-f"])).ok()?;
        let focused = focus
            .success()
            .then(|| String::from_utf8_lossy(&focus.stdout).trim().to_string());
        Some(vec![Some(format!("{tree:?}")), focused])
    }

    /// X11 window metadata has no secure-input role: never consulted.
    fn focus_security(&self, _target: Target) -> Security {
        Security::Unverifiable
    }

    fn click(&self, window_id: Target, point: Pair, button: MouseButton, count: u32) -> Result<()> {
        self.require_display()?;
        let xdotool = self.tool("xdotool")?;
        self.mousemove(&xdotool, window_id, point)?;
        let mut command = vec![xdotool, "click".to_string()];
        if count > 1 {
            command.extend(["--repeat".to_string(), count.to_string()]);
        }
        command.push(
            match button {
                MouseButton::Left => "1",
                MouseButton::Middle => "2",
                MouseButton::Right => "3",
            }
            .to_string(),
        );
        self.run_checked(&command, "click")
    }

    /// The pointer jumps: xdotool moves in one step, so apps wanting motion
    /// along the path see none.
    fn drag(&self, window_id: Target, from: Pair, to: Pair) -> Result<()> {
        self.require_display()?;
        let xdotool = self.tool("xdotool")?;
        self.mousemove(&xdotool, window_id, from)?;
        self.run_checked(&argv(&[&xdotool, "mousedown", "1"]), "mousedown")?;
        self.mousemove(&xdotool, window_id, to)?;
        self.run_checked(&argv(&[&xdotool, "mouseup", "1"]), "mouseup")
    }

    /// Wheel clicks (X11 carries no pixel deltas): 10 per page, roughly the
    /// macOS 800-pixel page.
    fn scroll(
        &self,
        window_id: Target,
        direction: ScrollDirection,
        pages: u32,
        point: Pair,
    ) -> Result<()> {
        self.require_display()?;
        let xdotool = self.tool("xdotool")?;
        self.mousemove(&xdotool, window_id, point)?;
        let button = match direction {
            ScrollDirection::Up => "4",
            ScrollDirection::Down => "5",
            ScrollDirection::Left => "6",
            ScrollDirection::Right => "7",
        };
        let repeat = (pages * WHEEL_CLICKS_PER_PAGE).to_string();
        let delay = WHEEL_REPEAT_DELAY_MS.to_string();
        self.run_checked(
            &argv(&[
                &xdotool, "click", "--repeat", &repeat, "--delay", &delay, button,
            ]),
            "click",
        )
    }

    /// Synthetic input to the bound window without activating it; apps
    /// that ignore synthetic events do not receive it (the plain form
    /// would deliver to whatever holds focus, so it is never retried).
    fn press_key(&self, window_id: Target, chord: &ParsedChord) -> Result<()> {
        self.require_display()?;
        let command = argv(&[
            &self.tool("xdotool")?,
            "key",
            "--window",
            &window_id.to_string(),
            &chord_keysym(chord),
        ]);
        self.run_checked(&command, "key")
    }

    fn type_text(&self, window_id: Target, text: &str) -> Result<()> {
        self.require_display()?;
        let delay = TYPE_DELAY_MS.to_string();
        let command = argv(&[
            &self.tool("xdotool")?,
            "type",
            "--window",
            &window_id.to_string(),
            "--delay",
            &delay,
            text,
        ]);
        self.run_checked(&command, "type")
    }

    fn capture(&self, request: CaptureRequest) -> Result<Captured> {
        match request {
            CaptureRequest::Window(window_id) => self.screenshot(window_id),
            CaptureRequest::Region { .. } => Err(ComputerUseError::new(
                crate::error::ErrorCode::ActionUnsupported,
                "region capture is not available on the Linux X11 backend",
            )),
        }
    }
}

/// The host's X11 backend.
#[cfg(target_os = "linux")]
pub(crate) mod native {
    use std::path::Path;

    use super::X11Platform;
    use crate::capture::CaptureDir;
    use crate::process::SystemTools;

    pub(crate) fn platform(agent_dir: &Path) -> X11Platform<SystemTools> {
        X11Platform::new(SystemTools, CaptureDir::under_agent_dir(agent_dir))
    }
}

#[cfg(test)]
mod tests;
