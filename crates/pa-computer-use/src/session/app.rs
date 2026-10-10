//! One bound app's state and the checks every App method shares: the guard,
//! element lookup with its freshness check, coordinate mapping, observation
//! and rendering, and the post-action settle.

use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use super::{InstructionsDir, Session};
use crate::element::{flatten, Element, Observation, Pair, Rect};
use crate::error::{
    head, invalid, not_running, transport, unsupported, ComputerUseError, ErrorCode, Result,
};
use crate::permissions::PermissionState;
use crate::platform::{Discovery, Platform, PlatformKind, Target};
use crate::policy::GateVerdict;
use crate::pyfmt::{fixed0, repr_str};
use crate::render::{diff, serialize};

/// One App method's point argument: an `(x, y)` tuple of numbers, or
/// anything else (its Python `repr` is what the error quotes).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PointArg {
    Valid { x: f64, y: f64, repr: String },
    Invalid { repr: String },
}

/// One App method's element-index argument.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IndexArg {
    Valid(i64),
    /// Not an integer: the Python type name.
    Invalid(String),
}

/// The last screenshot of the window: its pixel size, the logical rect it
/// covered, and the window it was of.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Shot {
    pub size: Pair,
    pub rect: Rect,
    pub window_id: Option<i64>,
}

/// One bound app.
pub(crate) struct AppState<E> {
    pub bundle_id: String,
    pub name: String,
    /// The pid on macOS, the window id on X11 and Wayland.
    pub target: Target,
    pub instructions_dir: InstructionsDir,
    pub observation: Option<Observation<E>>,
    pub lines: Option<Vec<String>>,
    pub state: Option<String>,
    pub shot: Option<Shot>,
}

impl<E> AppState<E> {
    pub(crate) fn new(
        bundle_id: String,
        name: String,
        target: Target,
        instructions_dir: InstructionsDir,
    ) -> Self {
        Self {
            bundle_id,
            name,
            target,
            instructions_dir,
            observation: None,
            lines: None,
            state: None,
            shot: None,
        }
    }
}

const NO_WINDOW_OBSERVED: &str = "no focused window observed; call get_ax_state() first";

fn invalid_point(repr: &str) -> ComputerUseError {
    invalid(format!(
        "point must be an (x, y) pair of numbers, got {repr}"
    ))
    .with_details(json!({"point": head(repr, 64)}))
}

fn outside_image(repr: &str, shot: Pair) -> ComputerUseError {
    invalid(format!(
        "point {repr} is outside the captured image ({}x{}); use coordinates from its screenshot",
        fixed0(shot.0),
        fixed0(shot.1)
    ))
    .with_details(json!({"point": head(repr, 64)}))
}

fn within(value: f64, limit: f64) -> bool {
    (0.0..limit).contains(&value)
}

impl<P: Platform> Session<P> {
    /// Re-validate the binding, the allowlist gate, the locked screen and
    /// (macOS) the Accessibility grant before an App method runs.
    ///
    /// A pid now owned by another process (or nothing) and a window that no
    /// longer is one of the app's windows fail closed.
    pub(crate) fn guard(&self, app: &AppState<P::Element>) -> Result<()> {
        let pid = app.target;
        match self.platform.discovery() {
            Discovery::Workspace(workspace) => {
                let running = workspace
                    .running_apps()?
                    .into_iter()
                    .find(|running| running.pid == pid)
                    .map(|running| running.bundle_id);
                match running {
                    None => {
                        return Err(not_running(format!(
                            "pid {pid} is no longer a running app; call get_app again to re-bind it"
                        ))
                        .with_details(json!({"pid": pid})));
                    }
                    Some(running) if running != app.bundle_id => {
                        return Err(ComputerUseError::new(
                            ErrorCode::AppNotAllowed,
                            format!(
                                "pid {pid} now belongs to {running}, not the bound {}; call \
                                 get_app again to re-bind the app you want",
                                app.bundle_id
                            ),
                        )
                        .with_details(json!({"pid": pid, "running_bundle_id": running})));
                    }
                    Some(_) => {}
                }
            }
            Discovery::Windows(windows) => {
                if !windows.owns(pid, &app.bundle_id)? {
                    return Err(not_running(format!(
                        "window {pid} is no longer one of {}'s windows; call get_app again to \
                         re-bind it",
                        app.bundle_id
                    ))
                    .with_details(json!({"pid": pid, "bundle_id": app.bundle_id})));
                }
            }
        }
        self.gate(&app.bundle_id)?;
        self.refuse_locked()?;
        if self.platform.kind() == PlatformKind::Mac {
            let accessibility = self.platform.permissions().accessibility();
            if accessibility != PermissionState::Ok {
                return Err(ComputerUseError::new(
                    ErrorCode::PermissionsNotGranted,
                    "the Accessibility grant is missing or was revoked; allow Prime Agent again \
                     in System Settings > Privacy & Security > Accessibility and restart Prime \
                     Agent, then re-bind with get_app",
                )
                .with_details(json!({
                    "permission": "accessibility",
                    "reported": accessibility.as_str(),
                })));
            }
        }
        Ok(())
    }

    /// Gate one app id against the settings file.
    pub(crate) fn gate(&self, bundle_id: &str) -> Result<()> {
        match self.context.policy.gate(bundle_id) {
            GateVerdict::Allowed { .. } => Ok(()),
            GateVerdict::Denied { reason, .. } => {
                Err(ComputerUseError::new(ErrorCode::AppNotAllowed, reason)
                    .with_details(json!({"bundle_id": bundle_id})))
            }
        }
    }

    pub(crate) fn refuse_locked(&self) -> Result<()> {
        if self.platform.screen_locked() {
            return Err(ComputerUseError::new(
                ErrorCode::ScreenLocked,
                "the screen is locked; ask the user to unlock it before driving apps",
            ));
        }
        Ok(())
    }

    /// The indexed element and its live handle from the current snapshot,
    /// refusing a stale index and an element that changed since.
    pub(crate) fn element(
        &self,
        app: &AppState<P::Element>,
        index: &IndexArg,
    ) -> Result<(usize, Element, P::Element)> {
        let index = match index {
            IndexArg::Valid(index) => *index,
            IndexArg::Invalid(type_name) => {
                return Err(
                    invalid(format!("element index must be an integer, got {type_name}"))
                        .with_details(json!({"element_index": type_name})),
                );
            }
        };
        let stale = || {
            ComputerUseError::new(
                ErrorCode::ElementStale,
                format!(
                    "element index {index} is stale; re-observe with get_ax_state() and use \
                     fresh indices"
                ),
            )
            .with_details(json!({"element_index": index}))
        };
        let observation = app.observation.as_ref();
        let refs_len = observation.map_or(0, |observation| observation.refs.len());
        let position = usize::try_from(index)
            .ok()
            .filter(|&position| position < refs_len);
        let (Some(position), Some(observation)) = (position, observation) else {
            return Err(stale());
        };
        let element = flatten(&observation.tree)
            .get(position)
            .map(|element| (*element).clone())
            .ok_or_else(stale)?;
        let handle = observation.refs[position].clone();
        let (live_role, live_title) = self.platform.live_fingerprint(&handle)?;
        if live_role != element.role || live_title != element.title {
            let shown =
                |value: &Option<String>| value.as_deref().map_or("None".to_string(), repr_str);
            return Err(ComputerUseError::new(
                ErrorCode::ElementStale,
                format!(
                    "element {position} changed since the last observation ({} -> {}); \
                     re-observe with get_ax_state()",
                    shown(&element.role),
                    shown(&live_role)
                ),
            )
            .with_details(json!({"element_index": position})));
        }
        Ok((position, element, handle))
    }

    /// One element's center from its observed geometry (screen space on
    /// macOS and in X11 observations, window-relative on Wayland).
    fn element_center(&self, app: &AppState<P::Element>, index: &IndexArg) -> Result<Pair> {
        let (position, element, _) = self.element(app, index)?;
        match (element.position, element.size) {
            (Some((x, y)), Some((width, height))) => Ok((x + width / 2.0, y + height / 2.0)),
            _ => Err(unsupported(format!(
                "element {position} has no on-screen position (web views often omit element \
                 geometry); use keyboard navigation, or window-screenshot coordinates from \
                 get_screenshot()"
            ))
            .with_details(json!({"element_index": position}))),
        }
    }

    fn observed_rect(app: &AppState<P::Element>) -> Result<Rect> {
        app.observation
            .as_ref()
            .and_then(|observation| observation.window_rect)
            .ok_or_else(|| transport(NO_WINDOW_OBSERVED))
    }

    /// The input point of one element, in the backend's input space: screen
    /// space on macOS, window-relative on X11 and Wayland.
    pub(crate) fn element_point(
        &self,
        app: &AppState<P::Element>,
        index: &IndexArg,
    ) -> Result<Pair> {
        let center = self.element_center(app, index)?;
        match self.platform.kind() {
            PlatformKind::Mac | PlatformKind::Wayland => Ok(center),
            PlatformKind::X11 => {
                let rect = Self::observed_rect(app)?;
                Ok((center.0 - rect.x, center.1 - rect.y))
            }
        }
    }

    /// One window-screenshot point in the backend's input space.
    pub(crate) fn image_point(&self, app: &AppState<P::Element>, point: &PointArg) -> Result<Pair> {
        let (x, y, repr) = match point {
            PointArg::Valid { x, y, repr } => (*x, *y, repr.as_str()),
            PointArg::Invalid { repr } => return Err(invalid_point(repr)),
        };
        match self.platform.kind() {
            PlatformKind::Mac => Self::screen_point(app, (x, y), repr),
            PlatformKind::X11 => {
                let rect = Self::observed_rect(app)?;
                if !within(x, rect.width) || !within(y, rect.height) {
                    return Err(outside_window(repr, rect));
                }
                Ok((x, y))
            }
            PlatformKind::Wayland => {
                // A screenshot of this window scales its pixels back to the
                // logical rect it covers, relative to the window's origin;
                // without one the point is logical already.
                match app.shot {
                    Some(shot) if shot.window_id == Some(app.target) => {
                        if !within(x, shot.size.0) || !within(y, shot.size.1) {
                            return Err(outside_image(repr, shot.size));
                        }
                        Ok((
                            shot.rect.x + x * shot.rect.width / shot.size.0,
                            shot.rect.y + y * shot.rect.height / shot.size.1,
                        ))
                    }
                    _ => Ok((x, y)),
                }
            }
        }
    }

    /// macOS: a window-screenshot point to screen space, scaling a Retina
    /// capture's pixels back to the window's logical bounds.
    fn screen_point(app: &AppState<P::Element>, (x, y): Pair, repr: &str) -> Result<Pair> {
        let rect = Self::observed_rect(app)?;
        let current_window = app
            .observation
            .as_ref()
            .and_then(|observation| observation.window_id);
        if let (Some(shot), Some(current)) = (app.shot, current_window) {
            if shot
                .window_id
                .is_some_and(|shot_window| shot_window != current)
            {
                return Err(transport(
                    "the focused window changed since the screenshot; take a fresh screenshot \
                     before clicking image coordinates",
                )
                .with_details(json!({})));
            }
        }
        if let Some(shot) = app.shot {
            if shot.size != (shot.rect.width, shot.rect.height) {
                // The pixel-to-logical scale is a size property: it survives
                // a moved window (the origin below is the live one).
                if !within(x, shot.size.0) || !within(y, shot.size.1) {
                    return Err(outside_image(repr, shot.size));
                }
                let scaled = (
                    x * shot.rect.width / shot.size.0,
                    y * shot.rect.height / shot.size.1,
                );
                if !within(scaled.0, rect.width) || !within(scaled.1, rect.height) {
                    return Err(invalid(format!(
                        "point {repr} lands outside the observed window ({}x{}); the window \
                         changed since the capture, so take a fresh screenshot",
                        fixed0(rect.width),
                        fixed0(rect.height)
                    ))
                    .with_details(json!({"point": head(repr, 64)})));
                }
                return Ok((rect.x + scaled.0, rect.y + scaled.1));
            }
            if !within(x, shot.size.0) || !within(y, shot.size.1) {
                return Err(outside_image(repr, shot.size));
            }
        }
        if !within(x, rect.width) || !within(y, rect.height) {
            return Err(outside_window(repr, rect));
        }
        Ok((rect.x + x, rect.y + y))
    }

    /// Observe the app and store the new snapshot, returning its text: the
    /// diff against the previous snapshot, or the full render.
    pub(crate) fn refresh(&self, app: &mut AppState<P::Element>, diff_on: bool) -> Result<String> {
        let observation = self.platform.observe(app.target)?;
        let lines = serialize(&observation.tree);
        let full = self.render_full(app, &observation, &lines);
        let text = match (&app.lines, diff_on) {
            (Some(previous), true) => {
                let changes = diff(previous, &lines);
                if changes.is_empty() {
                    "(no changes since the previous observation)".to_string()
                } else {
                    changes
                }
            }
            _ => full,
        };
        app.observation = Some(observation);
        app.lines = Some(lines);
        app.state = Some(text.clone());
        Ok(text)
    }

    /// The full state text, with the per-app instructions on the host's
    /// first observation of the app that has elements.
    fn render_full(
        &self,
        app: &AppState<P::Element>,
        observation: &Observation<P::Element>,
        lines: &[String],
    ) -> String {
        let mut header = format!("{} ({})", app.name, app.bundle_id);
        let count = observation.refs.len();
        if let Some(title) = &observation.window_title {
            let _ = write!(header, " — window {}", repr_str(title));
        }
        if count > 0 {
            let _ = write!(header, " — {count} elements, indices [0]..[{}]", count - 1);
        } else if observation.window_title.is_none() {
            header.push_str(" — no focused window");
        }
        if observation.truncated {
            header.push_str(
                " — TRUNCATED: the observation stopped at its element/depth/time bounds, some \
                 controls are hidden",
            );
        }
        let body = lines.join("\n");
        let mut text = [header.as_str(), body.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if count > 0 && self.context.first_instructions(&app.bundle_id) {
            if let Some(instructions) =
                load_instructions(app.instructions_dir.as_deref(), &app.bundle_id)
            {
                text.push('\n');
                text.push_str(&instructions);
            }
        }
        text
    }

    /// Wait for the app to process injected input: poll the focused
    /// window's fingerprint until two consecutive reads agree, the read
    /// fails, or the cap passes.
    pub(crate) fn settle(&self, target: Target) {
        let timing = self.context.timing;
        let deadline = Instant::now() + timing.settle_max;
        let Some(mut previous) = self.platform.window_fingerprint(target, timing.settle_max) else {
            return;
        };
        while Instant::now() < deadline {
            std::thread::sleep(timing.settle_poll);
            let budget = deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(50));
            match self.platform.window_fingerprint(target, budget) {
                Some(current) if current != previous => previous = current,
                Some(_) | None => return,
            }
        }
    }
}

fn outside_window(repr: &str, rect: Rect) -> ComputerUseError {
    invalid(format!(
        "point {repr} is outside the observed window ({}x{}); use coordinates from its screenshot",
        fixed0(rect.width),
        fixed0(rect.height)
    ))
    .with_details(json!({"point": head(repr, 64)}))
}

/// One app's usage guide, `<dir>/<bundle id>.md` with every character
/// outside `[A-Za-z0-9.-]` replaced by `_`, tolerating a missing, empty or
/// unreadable file.
pub(crate) fn load_instructions(dir: Option<&Path>, bundle_id: &str) -> Option<String> {
    let sanitized: String = bundle_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let text = std::fs::read_to_string(dir?.join(format!("{sanitized}.md"))).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}
