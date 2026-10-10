//! The Wayland backend for niri: niri IPC, AT-SPI, virtual input, grim.
//!
//! Selected under a niri session (`WAYLAND_DISPLAY` plus a live
//! `NIRI_SOCKET`). Windows come from niri's IPC, observation, focus and
//! secure-field detection from AT-SPI (pure Rust over zbus, replacing the
//! Python skill's PyGObject/libatspi), element actions run through AT-SPI
//! without moving focus, pointer and keyboard input through the
//! compositor's virtual-input protocols after the bound window is focused
//! and verified, and screenshots through the computer-use niri fork's
//! `CaptureWindow` request (upstream niri's screenshot action always copies
//! to the user's clipboard, so there `grim` captures the window's logical
//! rect). On the fork, coordinate input also waits for niri's animations to
//! settle and is checked against the compositor's own hit test. `ydotool`
//! is deliberately not used: its socket lets anything type as the user, its
//! absolute motion is not pixel-accurate, its typing is US-ASCII.

pub(crate) mod atspi;
pub(crate) mod input;
pub(crate) mod niri;

#[cfg(target_os = "linux")]
mod bus;
#[cfg(target_os = "linux")]
mod wl;

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use self::atspi::{Accessibility, AtSpi};
use self::input::{chord_stroke, keysym_for_char, KeyStroke, PointerTarget, VirtualInput};
use self::niri::{Niri, NiriTransport, WindowGeometry, WindowRecord};
use crate::capture::CaptureDir;
use crate::element::{cap, Observation, Pair, Rect};
use crate::error::{
    head, invalid, not_running, transport, unsupported, ComputerUseError, ErrorCode, Result,
};
use crate::keymap::ParsedChord;
use crate::permissions::{PermissionReport, PermissionState, WAYLAND_APPS_HELP_LINE};
use crate::platform::logind;
use crate::platform::{
    AppEntry, CaptureRequest, Captured, Discovery, ElementActions, FieldFocus, Fingerprint,
    FocusControl, MouseButton, Platform, PlatformKind, ScrollDirection, Target, WindowCandidate,
    WindowDirectory,
};
use crate::process::{optional_tool, run_tool, Tools, TOOL_TIMEOUT};
use crate::pyfmt::{casefold, fixed0, repr_float};
use crate::secure::{refuse_secure_focus, Security};
use crate::spec::{is_blank, AppSpec, SpecKey, SpecShape};

const MAX_OBSERVE: Duration = Duration::from_secs(3);
const MAX_FOCUS_SEARCH: Duration = Duration::from_secs(1);
/// Inside the App's 0.5 s settle budget.
const FINGERPRINT_FOCUS_SEARCH: Duration = Duration::from_millis(250);
const FOCUS_POLL: Duration = Duration::from_millis(20);
/// How long coordinate input and captures wait for niri's animations to
/// settle (polled every `FOCUS_POLL`).
const SETTLE_WAIT: Duration = Duration::from_secs(2);
const WHEEL_CLICKS_PER_PAGE: u32 = 10;
/// The element default actions that stand in for a click (`AXPress`).
const PRESS_ACTIONS: [&str; 6] = ["click", "press", "activate", "jump", "toggle", "open"];

fn absolute(name: &str) -> Option<&'static str> {
    match name {
        "grim" => Some("/usr/bin/grim"),
        "loginctl" => Some("/usr/bin/loginctl"),
        _ => None,
    }
}

/// The Wayland backend over its seams.
pub(crate) struct WaylandPlatform<T: Tools, N: NiriTransport, A: AtSpi, I: VirtualInput> {
    tools: T,
    niri: Niri<N>,
    accessibility: Accessibility<A>,
    input: I,
    capture: CaptureDir,
    /// How long input waits for niri to report the focus landed.
    focus_wait: Duration,
    /// Whether niri is the computer-use fork (it renders captures itself),
    /// probed once.
    fork: std::sync::OnceLock<bool>,
}

impl<T: Tools, N: NiriTransport, A: AtSpi, I: VirtualInput> WaylandPlatform<T, N, A, I> {
    pub(crate) fn new(
        tools: T,
        niri: N,
        bus: A,
        input: I,
        capture: CaptureDir,
        focus_wait: Duration,
    ) -> Self {
        Self {
            tools,
            niri: Niri { transport: niri },
            accessibility: Accessibility { bus },
            input,
            capture,
            focus_wait,
            fork: std::sync::OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn bus(&self) -> &A {
        &self.accessibility.bus
    }

    /// The bound window, `APP_NOT_RUNNING` when niri no longer has it.
    fn require_window(&self, window_id: Target) -> Result<WindowRecord> {
        self.niri.window(window_id)?.ok_or_else(|| {
            not_running(format!(
                "window {window_id} is no longer open; call get_app again to re-bind it"
            ))
            .with_details(json!({"window_id": window_id}))
        })
    }

    fn pid(window: &WindowRecord) -> Option<i64> {
        window.get("pid").and_then(Value::as_i64)
    }

    /// Focus the bound window and verify the focus landed: input on Wayland
    /// is focus-bound, so nothing is sent unless niri reports the window
    /// focused within the wait.
    fn focus_window(&self, window_id: Target) -> Result<()> {
        if self.niri.focused_window_id()? == Some(window_id) {
            return Ok(());
        }
        self.niri.focus(window_id)?;
        let deadline = Instant::now() + self.focus_wait;
        loop {
            if self.niri.focused_window_id()? == Some(window_id) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ComputerUseError::new(
                    ErrorCode::InjectionFailed,
                    format!(
                        "focus did not land on window {window_id}; no input was sent. Re-observe \
                         and retry"
                    ),
                )
                .with_details(json!({"window_id": window_id})));
            }
            std::thread::sleep(FOCUS_POLL);
        }
    }

    /// Whether the app's live focused element is a password field, failing
    /// closed (`Unverifiable`) when AT-SPI, the app, its frames or the
    /// element's role cannot be read, or the bounded search stopped early.
    fn live_focus_security(&self, window_id: Target) -> Security {
        let Ok(Some(window)) = self.niri.window(window_id) else {
            return Security::Unverifiable;
        };
        if self.accessibility.bus.available().is_err() {
            return Security::Unverifiable;
        }
        let Some(app) = self.accessibility.application(Self::pid(&window)) else {
            return Security::Unverifiable;
        };
        let roots = self.accessibility.focus_roots(&app, &window);
        if roots.is_empty() {
            return Security::Unverifiable;
        }
        match self
            .accessibility
            .find_focused(&roots, Instant::now() + MAX_FOCUS_SEARCH)
        {
            (None, true) => Security::NotSecure,
            (None, false) => Security::Unverifiable,
            (Some(focused), _) => {
                Security::from_probe(self.accessibility.bus.is_password(&focused))
            }
        }
    }

    /// Poll niri's geometry of the window until no animation moves it
    /// (within `SETTLE_WAIT`); `None` on a niri without
    /// `WindowGeometry`. A window still moving at the deadline fails with
    /// `code`.
    fn settle(&self, window_id: Target, code: ErrorCode) -> Result<Option<WindowGeometry>> {
        let deadline = Instant::now() + SETTLE_WAIT;
        loop {
            let Some(geometry) = self.niri.window_geometry(window_id)? else {
                return Ok(None);
            };
            if !geometry.animating {
                return Ok(Some(geometry));
            }
            if Instant::now() >= deadline {
                return Err(ComputerUseError::new(
                    code,
                    format!(
                        "window {window_id} was still animating after {} ms; nothing was sent \
                         or captured. Retry once it has settled",
                        SETTLE_WAIT.as_millis()
                    ),
                )
                .with_details(json!({"window_id": window_id})));
            }
            std::thread::sleep(FOCUS_POLL);
        }
    }

    /// Focus the bound window, let it settle, and map window-relative
    /// logical points onto its output for the virtual pointer. A point
    /// outside the window, or a window whose position niri hides, is refused
    /// before focus moves; a point that is off screen or that the
    /// compositor's hit test gives to something else is refused after.
    fn aim<const K: usize>(
        &self,
        window_id: Target,
        points: [Pair; K],
    ) -> Result<(PointerTarget, [Pair; K])> {
        let geometry = self.niri.geometry(&self.require_window(window_id)?)?;
        let rendered = self.niri.window_geometry(window_id)?.is_some();
        if !rendered && (geometry.origin.is_none() || geometry.output_rect.is_none()) {
            return Err(unsupported(format!(
                "coordinate input needs the window's screen position: {}. Use element actions by \
                 index (they run through AT-SPI), or ask the user to make the window floating",
                geometry.reason
            ))
            .with_details(json!({"platform": "wayland", "window_id": window_id})));
        }
        for (x, y) in points {
            if !(0.0..geometry.width).contains(&x) || !(0.0..geometry.height).contains(&y) {
                let repr = format!("({}, {})", repr_float(x), repr_float(y));
                return Err(invalid(format!(
                    "point {repr} is outside the window ({}x{})",
                    fixed0(geometry.width),
                    fixed0(geometry.height)
                ))
                .with_details(json!({"point": head(&repr, 64)})));
            }
        }
        self.focus_window(window_id)?;
        let placement = self.placement(window_id)?;
        let mut mapped = points;
        for point in &mut mapped {
            *point = self.map_point(window_id, &placement, *point)?;
        }
        #[allow(clippy::cast_possible_truncation)] // output sizes are far inside i32
        let target = PointerTarget {
            output: placement.output,
            width: placement.output_rect.width as i32,
            height: placement.output_rect.height as i32,
        };
        Ok((target, mapped))
    }

    /// Where the just-focused window sits once it settled: the rendered
    /// geometry, or the floating window's derived one on a niri without
    /// `WindowGeometry`. A window that is not on screen, or whose position
    /// or output geometry is unknown, is refused.
    fn placement(&self, window_id: Target) -> Result<Placement> {
        let settled = self.settle(window_id, ErrorCode::InjectionFailed)?;
        let (origin, visible, output, output_rect) = if let Some(settled) = settled {
            let Some(rect) = settled.live_rect() else {
                let why = if settled.overview_open {
                    "the overview is open"
                } else {
                    "its workspace is not shown"
                };
                return Err(refused(
                    window_id,
                    &format!("window {window_id} is not on screen after focusing it ({why})"),
                ));
            };
            let Some(visible) = settled.visible_rect else {
                return Err(refused(
                    window_id,
                    &format!("window {window_id} is scrolled fully off screen"),
                ));
            };
            let output_rect = match settled.output.as_deref() {
                Some(name) => self.niri.output_rect(name)?,
                None => None,
            };
            (
                (rect.x, rect.y),
                Some(Rect::from(visible)),
                settled.output,
                output_rect,
            )
        } else {
            let derived = self.niri.geometry(&self.require_window(window_id)?)?;
            let Some(origin) = derived.origin else {
                return Err(refused(
                    window_id,
                    &format!(
                        "coordinate input needs the window's screen position: {}",
                        derived.reason
                    ),
                ));
            };
            (origin, None, derived.output, derived.output_rect)
        };
        let Some(output_rect) = output_rect else {
            return Err(refused(
                window_id,
                &format!("the output of window {window_id} has no known geometry"),
            ));
        };
        Ok(Placement {
            origin,
            visible,
            output,
            output_rect,
        })
    }

    /// One window-relative point as an output-relative one, refused when it
    /// is off screen or the compositor's hit test gives it to anything but
    /// the window's input area.
    fn map_point(&self, window_id: Target, placement: &Placement, point: Pair) -> Result<Pair> {
        let (x, y) = (placement.origin.0 + point.0, placement.origin.1 + point.1);
        let repr = format!("({}, {})", repr_float(point.0), repr_float(point.1));
        if let Some(visible) = placement.visible {
            if !(visible.x..visible.x + visible.width).contains(&x)
                || !(visible.y..visible.y + visible.height).contains(&y)
            {
                return Err(refused(
                    window_id,
                    &format!("point {repr} of the window is off screen"),
                ));
            }
        }
        if let Some(hit) = self.niri.window_at((x, y))? {
            if let Some(layer) = hit.layer {
                return Err(refused(
                    window_id,
                    &format!(
                        "point {repr} is covered by the {} layer surface",
                        head(&layer.namespace, 64)
                    ),
                ));
            }
            match hit.window_id {
                Some(id) if id == window_id && hit.is_input => {}
                Some(id) if id == window_id => {
                    return Err(refused(
                        window_id,
                        &format!("point {repr} is not in the window's input area"),
                    ));
                }
                Some(id) => {
                    return Err(refused(
                        window_id,
                        &format!("point {repr} is covered by window {id}"),
                    ));
                }
                None => {
                    return Err(refused(window_id, &format!("point {repr} hits no window")));
                }
            }
        }
        Ok((x - placement.output_rect.x, y - placement.output_rect.y))
    }

    /// Whether another on-screen floating window intersects `rect` (a
    /// focused floating window is drawn above the others).
    fn overlapped(&self, window: &WindowRecord, rect: Rect) -> Result<bool> {
        if window
            .get("is_focused")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(false);
        }
        for other in self.niri.windows()? {
            if other.get("id") == window.get("id")
                || other.get("workspace_id") != window.get("workspace_id")
            {
                continue;
            }
            let Some(other) = self.niri.geometry(&other)?.rect() else {
                continue;
            };
            if other.x < rect.x + rect.width
                && rect.x < other.x + other.width
                && other.y < rect.y + rect.height
                && rect.y < other.y + other.height
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The window rendered alone by niri's `CaptureWindow` (tiled, covered
    /// or off-screen windows included), else `grim` over the logical rect of
    /// a floating, uncovered window. The captured rect is window-relative:
    /// screenshot points map back through it.
    fn screenshot(&self, window_id: Target) -> Result<Captured> {
        let window = self.require_window(window_id)?;
        self.settle(window_id, ErrorCode::TransportError)?;
        let dir = self.capture.open()?;
        let (name, path) = dir.new_target()?;
        let path = path.to_string_lossy().into_owned();
        if let Some(capture) = self.niri.capture_window(window_id, &path)? {
            let (png_width, png_height) = dir.png_dimensions(&name)?;
            dir.make_private(&name);
            dir.sweep(Some(&name));
            let scale = capture.scale;
            let (offset_x, offset_y) = capture.window_offset_px;
            return Ok(Captured {
                path,
                width: png_width,
                height: png_height,
                logical_rect: Some(Rect::new(
                    -f64::from(offset_x) / scale,
                    -f64::from(offset_y) / scale,
                    f64::from(png_width) / scale,
                    f64::from(png_height) / scale,
                )),
            });
        }
        let geometry = self.niri.geometry(&window)?;
        let Some(rect) = geometry.rect() else {
            return Err(unsupported(format!(
                "the window cannot be captured: {}; use get_ax_state() instead",
                geometry.reason
            ))
            .with_details(json!({"platform": "wayland", "window_id": window_id})));
        };
        if self.overlapped(&window, rect)? {
            return Err(unsupported(
                "another window overlaps the bound window, so a region capture would include its \
                 content; use get_ax_state() instead",
            )
            .with_details(json!({"platform": "wayland", "window_id": window_id})));
        }
        let Some(grim) = optional_tool(&self.tools, "grim", absolute("grim")) else {
            return Err(transport(
                "computer use backend unavailable: the Wayland backend needs grim on PATH for \
                 screenshots",
            ));
        };
        #[allow(clippy::cast_possible_truncation)] // screen coordinates are far inside i64
        let [x, y, width, height] =
            [rect.x, rect.y, rect.width, rect.height].map(|value| value.round_ties_even() as i64);
        let output = run_tool(
            &self.tools,
            &[
                grim,
                "-g".to_string(),
                format!("{x},{y} {width}x{height}"),
                path.clone(),
            ],
            TOOL_TIMEOUT,
        )?;
        if !output.success() {
            return Err(transport(format!(
                "grim failed with exit code {}: {}",
                output.code,
                output.capped()
            )));
        }
        let (png_width, png_height) = dir.png_dimensions(&name)?;
        dir.make_private(&name);
        dir.sweep(Some(&name));
        #[allow(clippy::cast_precision_loss)] // screen coordinates are far inside f64's mantissa
        let captured = Rect::new(
            x as f64 - rect.x,
            y as f64 - rect.y,
            width as f64,
            height as f64,
        );
        Ok(Captured {
            path,
            width: png_width,
            height: png_height,
            logical_rect: Some(captured),
        })
    }

    /// Focus the window, re-check the live focus for a secure field, then
    /// send the strokes (the backend's own re-check after the focus moved).
    fn send_after_focus(&self, window_id: Target, strokes: &[KeyStroke]) -> Result<()> {
        self.focus_window(window_id)?;
        refuse_secure_focus(self.live_focus_security(window_id))?;
        self.input.send_keys(strokes)
    }
}

/// Where a focused, settled window sits for the virtual pointer, in global
/// logical pixels.
struct Placement {
    origin: Pair,
    /// The part of the window on its output (known on the niri fork only).
    visible: Option<Rect>,
    output: Option<String>,
    output_rect: Rect,
}

/// Coordinate input refused after the focus moved: nothing was sent.
fn refused(window_id: Target, reason: &str) -> ComputerUseError {
    unsupported(format!(
        "{reason}; no input was sent. Use element actions by index (they run through AT-SPI), \
         or re-observe and retry"
    ))
    .with_details(json!({"platform": "wayland", "window_id": window_id}))
}

/// The `app_id` one spec names (stripped), refusing launch-path shapes.
fn spec_app_id(spec: &AppSpec) -> Result<String> {
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
        "the app spec must be an app_id or a {\"bundle_id\"|\"name\"} dict; Wayland windows have \
         no launch paths",
    )
    .with_details(json!({"spec": head(&spec.display, 64)})))
}

fn app_id(window: &WindowRecord) -> Option<&str> {
    window
        .get("app_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

impl<T: Tools, N: NiriTransport, A: AtSpi, I: VirtualInput> WindowDirectory
    for WaylandPlatform<T, N, A, I>
{
    fn list_apps(&self) -> Result<Vec<AppEntry>> {
        let mut apps: Vec<AppEntry> = Vec::new();
        for window in self.niri.windows()? {
            if let Some(id) = app_id(&window) {
                if !apps.iter().any(|app| app.id == id) {
                    apps.push(AppEntry {
                        id: id.to_string(),
                        name: id.to_string(),
                    });
                }
            }
        }
        Ok(apps)
    }

    /// The spec's windows by `app_id` casefolded: the focused one first,
    /// then the most recently focused.
    fn resolve(&self, spec: &AppSpec) -> Result<Vec<WindowCandidate>> {
        let wanted = casefold(&spec_app_id(spec)?);
        let mut matches: Vec<WindowRecord> = self
            .niri
            .windows()?
            .into_iter()
            .filter(|window| {
                window
                    .get("app_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| casefold(id) == wanted)
            })
            .collect();
        let recency = |window: &WindowRecord| {
            let focused = window
                .get("is_focused")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let stamp = window.get("focus_timestamp");
            let part = |name: &str| {
                stamp
                    .and_then(|stamp| stamp.get(name))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            };
            (focused, part("secs") + part("nanos") / 1e9)
        };
        matches.sort_by(|a, b| {
            let (a, b) = (recency(a), recency(b));
            b.0.cmp(&a.0).then(b.1.total_cmp(&a.1))
        });
        Ok(matches
            .into_iter()
            .filter_map(|window| {
                Some(WindowCandidate {
                    app_id: window.get("app_id")?.as_str()?.to_string(),
                    window_id: window.get("id")?.as_i64()?,
                })
            })
            .collect())
    }

    fn owns(&self, window_id: i64, app_id: &str) -> Result<bool> {
        Ok(self
            .niri
            .window(window_id)?
            .is_some_and(|window| window.get("app_id").and_then(Value::as_str) == Some(app_id)))
    }
}

impl<T: Tools, N: NiriTransport, A: AtSpi, I: VirtualInput> ElementActions<A::Node>
    for WaylandPlatform<T, N, A, I>
{
    fn default_action(&self, actions: &[String]) -> Option<String> {
        let mut lowered: Vec<(String, &String)> = Vec::new();
        for action in actions {
            let key = casefold(action);
            match lowered.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = action,
                None => lowered.push((key, action)),
            }
        }
        PRESS_ACTIONS.iter().find_map(|wanted| {
            lowered
                .iter()
                .find(|(key, _)| key == wanted)
                .map(|(_, action)| (*action).clone())
        })
    }

    fn perform(&self, element: &A::Node, action: &str) -> Result<()> {
        let bus = &self.accessibility.bus;
        let short = || json!({"action": head(action, 32)});
        for index in 0..bus.action_count(element).unwrap_or(0) {
            if bus.action_name(element, index).as_deref() != Some(action) {
                continue;
            }
            return match bus.do_action(element, index) {
                Ok(true) => Ok(()),
                Ok(false) => {
                    Err(unsupported(format!("the element refused {action}")).with_details(short()))
                }
                Err(reason) => Err(unsupported(format!(
                    "the element did not perform {action}: {}",
                    head(&reason, crate::error::ERROR_LIMIT)
                ))
                .with_details(short())),
            };
        }
        Err(unsupported(format!("the element no longer exposes {action}")).with_details(short()))
    }

    fn is_settable(&self, element: &A::Node) -> bool {
        self.accessibility.is_settable(element)
    }

    fn current_value(&self, element: &A::Node) -> Option<String> {
        self.accessibility.current_value(element)
    }

    fn set_value(&self, element: &A::Node, value: &str) -> Result<()> {
        let bus = &self.accessibility.bus;
        if !bus.has_editable_text(element) {
            return Err(
                unsupported("the element does not accept text writes").with_details(json!({}))
            );
        }
        match bus.set_text_contents(element, value) {
            Ok(true) => Ok(()),
            Ok(false) => {
                Err(unsupported("the element refused the text write").with_details(json!({})))
            }
            Err(reason) => Err(unsupported(format!(
                "setting the text failed: {}",
                head(&reason, crate::error::ERROR_LIMIT)
            ))
            .with_details(json!({}))),
        }
    }

    fn select_range(&self, element: &A::Node, location: usize, length: usize) -> Result<()> {
        let bus = &self.accessibility.bus;
        if !bus.has_text(element) {
            return Err(
                unsupported("the element exposes no text to select").with_details(json!({}))
            );
        }
        let start = i64::try_from(location).unwrap_or(i64::MAX);
        let end = i64::try_from(location + length).unwrap_or(i64::MAX);
        let selected = if bus.selection_count(element).unwrap_or(0) > 0 {
            bus.set_selection(element, start, end)
        } else {
            bus.add_selection(element, start, end)
        };
        match selected {
            Ok(true) => Ok(()),
            Ok(false) => Err(unsupported("the element refused the selection")
                .with_details(json!({"location": location, "length": length}))),
            Err(reason) => Err(unsupported(format!(
                "selecting the text failed: {}",
                head(&reason, crate::error::ERROR_LIMIT)
            ))
            .with_details(json!({}))),
        }
    }

    fn live_security(&self, element: &A::Node) -> Security {
        if self.accessibility.bus.available().is_err() {
            return Security::Unverifiable;
        }
        Security::from_probe(self.accessibility.bus.is_password(element))
    }

    fn field_focus(&self) -> Option<&dyn FieldFocus<A::Node>> {
        Some(self)
    }
}

impl<T: Tools, N: NiriTransport, A: AtSpi, I: VirtualInput> FieldFocus<A::Node>
    for WaylandPlatform<T, N, A, I>
{
    /// Editable text or a password field: clicking one means focusing it.
    fn is_text_field(&self, element: &A::Node) -> bool {
        self.accessibility.bus.is_password(element) == Some(true)
            || self.accessibility.is_settable(element)
    }

    /// Component.GrabFocus; GTK 4 does not implement it, which sends the
    /// caller to a real pointer click.
    fn grab_focus(&self, element: &A::Node) -> bool {
        self.accessibility.bus.grab_focus(element) == Some(true)
            && self
                .accessibility
                .bus
                .states(element)
                .is_some_and(|states| states.focused)
    }

    fn wait_focused(&self, element: &A::Node) -> bool {
        self.accessibility
            .wait_focused(element, Duration::from_millis(500))
    }
}

impl<T: Tools, N: NiriTransport, A: AtSpi, I: VirtualInput> FocusControl
    for WaylandPlatform<T, N, A, I>
{
    fn activate(&self, target: Target) -> Result<()> {
        self.focus_window(target)
    }

    fn is_frontmost(&self, target: Target) -> Result<bool> {
        Ok(self.niri.focused_window_id()? == Some(target))
    }
}

impl<T, N, A, I> Platform for WaylandPlatform<T, N, A, I>
where
    T: Tools + 'static,
    N: NiriTransport + 'static,
    A: AtSpi + 'static,
    I: VirtualInput + 'static,
{
    type Element = A::Node;

    fn kind(&self) -> PlatformKind {
        PlatformKind::Wayland
    }

    fn discovery(&self) -> Discovery<'_> {
        Discovery::Windows(self)
    }

    fn screen_locked(&self) -> bool {
        logind::screen_locked(&self.tools, absolute("loginctl"))
    }

    /// AT-SPI as `accessibility`, the niri fork's own capture or grim as
    /// `screen_recording`, the virtual
    /// pointer and keyboard as `input`, with a fix-it line for each gap.
    fn permissions(&self) -> PermissionReport {
        let mut help = Vec::new();
        let accessibility = match self.accessibility.bus.available() {
            Err(reason) => {
                help.push(reason);
                PermissionState::Missing
            }
            Ok(()) if self.accessibility.bus.applications().is_none() => {
                help.push(
                    "AT-SPI: the accessibility bus did not answer; check that at-spi2-core is \
                     running"
                        .to_string(),
                );
                PermissionState::Unknown
            }
            Ok(()) => PermissionState::Ok,
        };
        // The fork renders captures itself; a niri that refuses the probe
        // (or fails it, uncached) needs grim. The probe point is the first
        // output's origin.
        let fork = self.fork.get().copied().or_else(|| {
            let outputs = self.niri.response("Outputs").ok()?;
            let logical = outputs
                .as_object()
                .and_then(|outputs| outputs.values().next())
                .and_then(|output| output.get("logical"));
            let at = |name: &str| {
                logical
                    .and_then(|logical| logical.get(name))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            };
            let fork = self.niri.window_at((at("x"), at("y"))).ok()?.is_some();
            Some(*self.fork.get_or_init(|| fork))
        });
        let screen_recording = if fork == Some(true)
            || optional_tool(&self.tools, "grim", absolute("grim")).is_some()
        {
            PermissionState::Ok
        } else {
            help.push("Screenshots: install grim (niri implements wlr-screencopy)".to_string());
            PermissionState::Missing
        };
        let (pointer, keyboard) = match self.input.available() {
            Ok(available) => (
                PermissionState::from_probe(Some(available.pointer)),
                PermissionState::from_probe(Some(available.keyboard)),
            ),
            Err(_) => (PermissionState::Unknown, PermissionState::Unknown),
        };
        if pointer != PermissionState::Ok || keyboard != PermissionState::Ok {
            help.push(
                "Input: the compositor must offer zwlr_virtual_pointer_manager_v1 and \
                 zwp_virtual_keyboard_manager_v1 (niri does for clients outside a sandboxed \
                 security context)"
                    .to_string(),
            );
        }
        help.push(WAYLAND_APPS_HELP_LINE.to_string());
        PermissionReport::Wayland {
            accessibility,
            screen_recording,
            pointer,
            keyboard,
            help,
        }
    }

    /// niri's title, the absolute logical rect while the window is on screen
    /// (only for a floating one on a niri without `WindowGeometry`), and the
    /// AT-SPI frame's showing descendants (empty for an app off the bus).
    fn observe(&self, window_id: Target) -> Result<Observation<A::Node>> {
        let window = self.require_window(window_id)?;
        let window_rect = match self.niri.window_geometry(window_id)? {
            Some(geometry) => geometry.live_rect(),
            None => self.niri.geometry(&window)?.rect(),
        };
        self.accessibility.bus.available().map_err(transport)?;
        let frame = self
            .accessibility
            .application(Self::pid(&window))
            .and_then(|app| self.accessibility.frame(&app, &window));
        let walk = frame.map(|frame| {
            self.accessibility
                .walk(&frame, Instant::now() + MAX_OBSERVE)
        });
        let title = cap(window
            .get("title")
            .and_then(Value::as_str)
            .map(ToString::to_string));
        Ok(match walk {
            Some(walk) => Observation {
                window_title: title,
                tree: walk.tree,
                refs: walk.refs,
                window_rect,
                focused_index: walk.focused_index,
                window_id: Some(window_id),
                truncated: walk.truncated,
            },
            None => Observation {
                window_title: title,
                window_rect,
                window_id: Some(window_id),
                ..Observation::empty()
            },
        })
    }

    fn live_fingerprint(&self, element: &A::Node) -> Result<(Option<String>, Option<String>)> {
        Ok(self.accessibility.live_fingerprint(element))
    }

    /// niri's focused window and title, then from AT-SPI the frame's child
    /// count and the focused element's role, name and (non-secure) text head.
    fn window_fingerprint(&self, window_id: Target, _budget: Duration) -> Option<Fingerprint> {
        let window = self.niri.window(window_id).ok()?;
        let focused = self.niri.focused_window_id().ok()?;
        let window = window?;
        let mut fingerprint = vec![
            focused.map(|id| id.to_string()),
            window
                .get("title")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        ];
        if self.accessibility.bus.available().is_err() {
            fingerprint.extend([None, None, None, None]);
            return Some(fingerprint);
        }
        let roots = self
            .accessibility
            .application(Self::pid(&window))
            .map(|app| self.accessibility.focus_roots(&app, &window))
            .unwrap_or_default();
        fingerprint.extend(
            self.accessibility
                .focus_fingerprint(&roots, FINGERPRINT_FOCUS_SEARCH),
        );
        Some(fingerprint)
    }

    fn focus_security(&self, target: Target) -> Security {
        self.live_focus_security(target)
    }

    /// Focus the window, then click through the virtual pointer (see
    /// `aim` for what is refused, and when).
    fn click(&self, window_id: Target, point: Pair, button: MouseButton, count: u32) -> Result<()> {
        let (target, [local]) = self.aim(window_id, [point])?;
        self.input.click(&target, local, button, count)
    }

    fn drag(&self, window_id: Target, from: Pair, to: Pair) -> Result<()> {
        let (target, [start, end]) = self.aim(window_id, [from, to])?;
        self.input.drag(&target, start, end)
    }

    fn scroll(
        &self,
        window_id: Target,
        direction: ScrollDirection,
        pages: u32,
        point: Pair,
    ) -> Result<()> {
        let (target, [local]) = self.aim(window_id, [point])?;
        self.input
            .scroll(&target, local, direction, pages * WHEEL_CLICKS_PER_PAGE)
    }

    fn press_key(&self, window_id: Target, chord: &ParsedChord) -> Result<()> {
        let stroke = chord_stroke(chord)?;
        self.send_after_focus(window_id, &[stroke])
    }

    /// Every character through its own keysym (a newline presses Return).
    fn type_text(&self, window_id: Target, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let strokes = text
            .chars()
            .map(|character| keysym_for_char(character).map(KeyStroke::plain))
            .collect::<Result<Vec<_>>>()?;
        self.send_after_focus(window_id, &strokes)
    }

    fn capture(&self, request: CaptureRequest) -> Result<Captured> {
        match request {
            CaptureRequest::Window(window_id) => self.screenshot(window_id),
            CaptureRequest::Region { .. } => Err(unsupported(
                "region capture is not available on the Wayland backend",
            )),
        }
    }

    fn element_actions(&self) -> Option<&dyn ElementActions<A::Node>> {
        Some(self)
    }

    fn focus_control(&self) -> Option<&dyn FocusControl> {
        Some(self)
    }
}

/// The host's Wayland backend: the niri socket, the accessibility bus over
/// zbus, the virtual-input protocols over wayland-client.
#[cfg(target_os = "linux")]
pub(crate) mod native {
    use std::path::Path;
    use std::time::Duration;

    use super::bus::BusAtSpi;
    use super::niri::SocketTransport;
    use super::wl::{session_input, WaylandInput};
    use super::WaylandPlatform;
    use crate::capture::CaptureDir;
    use crate::process::SystemTools;

    pub(crate) type NativeWayland =
        WaylandPlatform<SystemTools, SocketTransport, BusAtSpi, WaylandInput>;

    pub(crate) fn platform(agent_dir: &Path) -> NativeWayland {
        WaylandPlatform::new(
            SystemTools,
            SocketTransport::default(),
            BusAtSpi::default(),
            session_input(),
            CaptureDir::under_agent_dir(agent_dir),
            Duration::from_millis(500),
        )
    }
}

#[cfg(test)]
mod fakes;
#[cfg(test)]
mod tests;
