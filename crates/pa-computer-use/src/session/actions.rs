//! The App methods: observation, capture, input and element actions.
//!
//! Every input or element action runs through [`Session::run_action`]: the
//! guard, the dispatch, the settle (for input the app processes
//! asynchronously), and one `computer_use_action` event. Argument checks
//! that depend only on the Python values run in the kernel client before
//! the request is sent; what needs the snapshot or the live UI runs here.

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use serde_json::{json, Value};

use super::app::{IndexArg, PointArg, Shot};
use super::{AppState, Session};
use crate::element::Rect;
use crate::error::{
    head, invalid, transport, unsupported, ComputerUseError, ErrorCode, Result, ERROR_LIMIT,
};
use crate::keymap::parse_chord;
use crate::permissions::PermissionState;
use crate::platform::{
    CaptureRequest, Captured, ElementActions, MouseButton, PasteFormat, Platform, PlatformKind,
    RecognizedText, ScrollDirection,
};
use crate::pyfmt::repr_str;
use crate::secure::{refuse_secure_focus, refuse_secure_write};
use crate::telemetry::Outcome;

/// One App method call, decoded from the kernel request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AppCall {
    GetAxState {
        diff: bool,
    },
    GetScreenshot,
    GetTextRegions,
    Click {
        target: TargetArg,
        button: MouseButton,
        count: u32,
    },
    Drag {
        from: PointArg,
        to: PointArg,
    },
    Scroll {
        target: TargetArg,
        direction: ScrollDirection,
        pages: u32,
    },
    PressKey {
        key: TextArg,
    },
    TypeText {
        text: TextArg,
    },
    SetValue {
        index: IndexArg,
        value: String,
    },
    SelectText {
        index: IndexArg,
        text: String,
        prefix: Option<String>,
        suffix: Option<String>,
    },
    SecondaryAction {
        index: IndexArg,
        action: ActionArg,
    },
    Paste {
        text: String,
        format: PasteFormat,
    },
    Activate,
    IsFrontmost,
}

/// A click or scroll target: an element index, a window-screenshot point,
/// or something else (its Python type name).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TargetArg {
    Index(i64),
    Point(PointArg),
    Invalid(String),
}

/// A string argument the backend validates: the string, or the Python
/// type name of the non-string the model passed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TextArg {
    Text(String),
    NotText(String),
}

/// A secondary action name: the string, or the `str()` of a non-string
/// (which never names an exposed action).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ActionArg {
    Name(String),
    NotText(String),
}

impl ActionArg {
    fn display(&self) -> &str {
        match self {
            ActionArg::Name(text) | ActionArg::NotText(text) => text,
        }
    }
}

/// One paste transaction at a time across the host (save, write, paste,
/// restore must not interleave).
static PASTE_LOCK: Mutex<()> = Mutex::new(());

const LINUX_FOCUS_GAP: &str = "focus control is not available on the linux X11 backend yet; \
                               keyboard flows that need app focus are unsupported there";

fn invalid_target(type_name: &str) -> ComputerUseError {
    invalid(format!(
        "target must be an element index or an (x, y) tuple, got {type_name}"
    ))
    .with_details(json!({"target": type_name}))
}

/// `ACTION_UNSUPPORTED` for an App action with no X11 backing.
fn x11_gap(action: &str, hint: &str) -> ComputerUseError {
    unsupported(format!(
        "{action} is not available on the Linux X11 backend: X11 exposes no clipboard or \
         element-value API for it; {hint}"
    ))
    .with_details(json!({"action": action, "platform": "linux"}))
}

/// `ACTION_UNSUPPORTED` for an App action with no Wayland backing.
fn wayland_gap(action: &str, reason: &str) -> ComputerUseError {
    unsupported(format!(
        "{action} is not available on the Wayland backend: {reason}"
    ))
    .with_details(json!({"action": action, "platform": "wayland"}))
}

fn screen_recording_missing(reported: PermissionState) -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::PermissionsNotGranted,
        "the Screen Recording grant is missing or unknown; allow Prime Agent in System Settings > \
         Privacy & Security > Screen Recording, then retry",
    )
    .with_details(json!({"permission": "screen_recording", "reported": reported.as_str()}))
}

fn captured_json(captured: &Captured) -> Value {
    json!({"path": captured.path, "width": captured.width, "height": captured.height})
}

/// Python's `int(round(value))` (ties to even).
fn round_int(value: f64) -> i64 {
    #[allow(clippy::cast_possible_truncation)] // screen coordinates are far inside i64
    let rounded = value.round_ties_even() as i64;
    rounded
}

/// The capture region of an observed macOS window.
fn region(rect: Rect, window_id: Option<i64>) -> CaptureRequest {
    CaptureRequest::Region {
        origin: (round_int(rect.x), round_int(rect.y)),
        size: (round_int(rect.width), round_int(rect.height)),
        window_id,
    }
}

/// Vision's bottom-left normalized boxes as top-left normalized regions,
/// sorted top-to-bottom then left-to-right, at most 400.
fn text_regions(observations: Vec<RecognizedText>) -> Vec<(String, f64, [f64; 4])> {
    const MAX_REGIONS: usize = 400;
    let unit = |value: f64| value.clamp(0.0, 1.0);
    let mut regions: Vec<(String, f64, [f64; 4])> = observations
        .into_iter()
        .map(|observation| {
            let (x, bottom, width, height) = observation.bbox;
            (
                observation.text,
                observation.confidence,
                [
                    unit(x),
                    unit(1.0 - bottom - height),
                    unit(width),
                    unit(height),
                ],
            )
        })
        .collect();
    regions.sort_by(|a, b| a.2[1].total_cmp(&b.2[1]).then(a.2[0].total_cmp(&b.2[0])));
    regions.truncate(MAX_REGIONS);
    regions
}

/// The occurrences of `text` in `value` (code-point offsets) constrained by
/// an immediate prefix and suffix; at most two (ambiguity is all a caller
/// needs to know).
fn occurrences(value: &[char], text: &[char], prefix: &[char], suffix: &[char]) -> Vec<usize> {
    let mut starts = Vec::new();
    if text.len() > value.len() {
        return starts;
    }
    for start in 0..=value.len() - text.len() {
        if value[start..start + text.len()] != *text {
            continue;
        }
        let end = start + text.len();
        let before_ok = prefix.is_empty()
            || (start >= prefix.len() && value[start - prefix.len()..start] == *prefix);
        let after_ok = suffix.is_empty() || value.get(end..end + suffix.len()) == Some(suffix);
        if before_ok && after_ok {
            starts.push(start);
            if starts.len() > 1 {
                break;
            }
        }
    }
    starts
}

impl<P: Platform> Session<P> {
    /// Run one guarded action, settle its UI effects, and emit telemetry.
    fn run_action(
        &self,
        app: &mut AppState<P::Element>,
        action: &'static str,
        settle: bool,
        dispatch: impl FnOnce(&Self, &mut AppState<P::Element>) -> Result<()>,
    ) -> Result<Value> {
        let started = Instant::now();
        let result = self
            .guard(app)
            .and_then(|()| dispatch(self, app))
            .map(|()| {
                if settle {
                    self.settle(app.target);
                }
            });
        let outcome = match &result {
            Ok(()) => Outcome::Ok,
            Err(error) => Outcome::Error(error.code),
        };
        self.context.emit_action(action, started, outcome);
        result.map(|()| Value::Null)
    }

    fn element_actions(&self) -> Result<&dyn ElementActions<P::Element>> {
        self.platform
            .element_actions()
            .ok_or_else(|| unsupported("element actions are not available on this backend"))
    }

    /// Dispatch one decoded App call.
    pub(crate) fn dispatch(&self, app: &mut AppState<P::Element>, call: AppCall) -> Result<Value> {
        match call {
            AppCall::GetAxState { diff } => self.get_ax_state(app, diff),
            AppCall::GetScreenshot => self.get_screenshot(app),
            AppCall::GetTextRegions => self.get_text_regions(app),
            AppCall::Click { target, button, count } => {
                self.run_action(app, "click", true, |session, app| session.click(app, &target, button, count))
            }
            AppCall::Drag { from, to } => self.run_action(app, "drag", true, |session, app| {
                let from = session.image_point(app, &from)?;
                let to = session.image_point(app, &to)?;
                session.platform.drag(app.target, from, to)
            }),
            AppCall::Scroll { target, direction, pages } => self.run_action(app, "scroll", true, |session, app| {
                let point = match &target {
                    TargetArg::Index(index) => session.element_point(app, &IndexArg::Valid(*index))?,
                    TargetArg::Point(point) => session.image_point(app, point)?,
                    TargetArg::Invalid(type_name) => return Err(invalid_target(type_name)),
                };
                session.platform.scroll(app.target, direction, pages, point)
            }),
            AppCall::PressKey { key } => self.run_action(app, "press_key", true, |session, app| {
                session.refuse_secure_focus(app)?;
                let chord = match &key {
                    TextArg::Text(key) => parse_chord(key)?,
                    TextArg::NotText(type_name) => {
                        return Err(invalid(format!("key chord must be a string, got {type_name}"))
                            .with_details(json!({"key": type_name})));
                    }
                };
                session.platform.press_key(app.target, &chord)
            }),
            AppCall::TypeText { text } => self.run_action(app, "type_text", true, |session, app| {
                session.refuse_secure_focus(app)?;
                let text = match &text {
                    TextArg::Text(text) => text,
                    TextArg::NotText(type_name) => {
                        return Err(invalid(format!("text must be a string, got {type_name}"))
                            .with_details(json!({"text": type_name})));
                    }
                };
                if text.is_empty() {
                    return Ok(());
                }
                session.platform.type_text(app.target, text)
            }),
            AppCall::SetValue { index, value } => {
                self.run_action(app, "set_value", false, |session, app| session.set_value(app, &index, &value))
            }
            AppCall::SelectText { index, text, prefix, suffix } => self.run_action(app, "select_text", false, |session, app| {
                session.select_text(app, &index, &text, prefix.as_deref(), suffix.as_deref())
            }),
            AppCall::SecondaryAction { index, action } => self.run_action(app, "secondary", true, |session, app| {
                if session.platform.kind() == PlatformKind::X11 {
                    return Err(x11_gap("perform_secondary_action", "click the element instead"));
                }
                let (position, element, handle) = session.element(app, &index)?;
                let exposed = match &action {
                    ActionArg::Name(name) => element.actions.contains(name),
                    ActionArg::NotText(_) => false,
                };
                if !exposed {
                    let listed = if element.actions.is_empty() {
                        "no actions".to_string()
                    } else {
                        element.actions.join(", ")
                    };
                    return Err(unsupported(format!(
                        "element {position} exposes {listed}, not {}",
                        action.display()
                    ))
                    .with_details(json!({"element_index": position, "action": head(action.display(), 32)})));
                }
                session.element_actions()?.perform(&handle, action.display())
            }),
            AppCall::Paste { text, format } => {
                self.run_action(app, "paste", true, |session, app| session.paste(app, &text, format))
            }
            AppCall::Activate => self.run_action(app, "activate", true, |session, app| {
                match session.platform.kind() {
                    PlatformKind::X11 => Err(unsupported(LINUX_FOCUS_GAP).with_details(json!({"platform": "linux"}))),
                    PlatformKind::Mac | PlatformKind::Wayland => session
                        .platform
                        .focus_control()
                        .ok_or_else(|| unsupported("focus control is not available on this backend"))?
                        .activate(app.target),
                }
            }),
            AppCall::IsFrontmost => match self.platform.kind() {
                PlatformKind::X11 => Err(unsupported(LINUX_FOCUS_GAP).with_details(json!({"platform": "linux"}))),
                PlatformKind::Mac | PlatformKind::Wayland => {
                    let frontmost = self
                        .platform
                        .focus_control()
                        .ok_or_else(|| unsupported("focus control is not available on this backend"))?
                        .is_frontmost(app.target)?;
                    Ok(Value::Bool(frontmost))
                }
            },
        }
    }

    /// `get_ax_state(diff)`: the guard's refusal emits no telemetry; the
    /// observation's outcome does, as a `get_state` action.
    fn get_ax_state(&self, app: &mut AppState<P::Element>, diff: bool) -> Result<Value> {
        self.guard(app)?;
        let started = Instant::now();
        match self.refresh(app, diff) {
            Ok(text) => {
                self.context.emit_action("get_state", started, Outcome::Ok);
                Ok(Value::String(text))
            }
            Err(error) => {
                self.context
                    .emit_action("get_state", started, Outcome::Error(error.code));
                Err(error)
            }
        }
    }

    fn get_screenshot(&self, app: &mut AppState<P::Element>) -> Result<Value> {
        self.guard(app)?;
        match self.platform.kind() {
            PlatformKind::Wayland => {
                let captured = self.platform.capture(CaptureRequest::Window(app.target))?;
                if let Some(rect) = captured.logical_rect {
                    app.shot = Some(Shot {
                        size: (f64::from(captured.width), f64::from(captured.height)),
                        rect,
                        window_id: Some(app.target),
                    });
                }
                Ok(captured_json(&captured))
            }
            PlatformKind::X11 => {
                let captured = self.platform.capture(CaptureRequest::Window(app.target))?;
                Ok(captured_json(&captured))
            }
            PlatformKind::Mac => {
                let reported = self.platform.permissions().screen_recording();
                if reported != PermissionState::Ok {
                    return Err(screen_recording_missing(reported));
                }
                // Snapshotted once: a concurrent re-observe cannot re-tag the image.
                let observed = app.observation.as_ref().and_then(|observation| {
                    observation
                        .window_rect
                        .map(|rect| (rect, observation.window_id))
                });
                let Some((rect, window_id)) = observed else {
                    return Err(transport(
                        "no focused window observed; call get_ax_state() first",
                    ));
                };
                let Some(window_id) = window_id else {
                    // A region capture would include whatever else is on
                    // screen there, so a window without a scoping id is refused.
                    return Err(transport(
                        "the focused window does not expose its window id, so the capture cannot \
                         be scoped to it; call get_ax_state() again and retry",
                    )
                    .with_details(json!({})));
                };
                let captured = self.platform.capture(region(rect, Some(window_id)))?;
                app.shot = Some(Shot {
                    size: (f64::from(captured.width), f64::from(captured.height)),
                    rect,
                    window_id: Some(window_id),
                });
                Ok(captured_json(&captured))
            }
        }
    }

    fn get_text_regions(&self, app: &mut AppState<P::Element>) -> Result<Value> {
        self.guard(app)?;
        let recognizer = match self.platform.kind() {
            PlatformKind::Wayland => {
                return Err(wayland_gap(
                    "get_text_regions",
                    "the OCR screen-reading path is macOS-only; use get_ax_state or \
                     get_screenshot instead",
                ));
            }
            PlatformKind::X11 => {
                return Err(unsupported(
                    "get_text_regions is not available on the Linux X11 backend yet: the OCR \
                     screen-reading path is macOS-only; use get_ax_state or get_screenshot instead",
                )
                .with_details(json!({"action": "get_text_regions", "platform": "linux"})));
            }
            PlatformKind::Mac => self
                .platform
                .text_recognizer()
                .ok_or_else(|| unsupported("text recognition is not available on this backend"))?,
        };
        let reported = self.platform.permissions().screen_recording();
        if reported != PermissionState::Ok {
            return Err(screen_recording_missing(reported));
        }
        let observed = app.observation.as_ref().and_then(|observation| {
            observation
                .window_rect
                .map(|rect| (rect, observation.window_id))
        });
        let Some((rect, window_id)) = observed else {
            return Err(transport(
                "no focused window observed; call get_ax_state() first",
            ));
        };
        let captured = self.platform.capture(region(rect, window_id))?;
        let observations = recognizer.recognize(&captured.path).map_err(|reason| {
            transport(format!(
                "vision text recognition failed: {}",
                head(&reason, ERROR_LIMIT)
            ))
        })?;
        let (width, height) = (f64::from(captured.width), f64::from(captured.height));
        let regions: Vec<Value> = text_regions(observations)
            .into_iter()
            .map(|(text, confidence, [x, y, w, h])| {
                json!({
                    "text": text,
                    "confidence": confidence,
                    "x": x * width,
                    "y": y * height,
                    "width": w * width,
                    "height": h * height,
                })
            })
            .collect();
        Ok(json!({
            "regions": regions,
            "width": captured.width,
            "height": captured.height,
            "path": captured.path,
        }))
    }

    /// Refuse keyboard entry into a focused secure field. X11 window
    /// metadata has no secure-input role, so the X11 backend cannot refuse
    /// anything (a documented platform gap; failing closed there would
    /// refuse every keystroke).
    fn refuse_secure_focus(&self, app: &AppState<P::Element>) -> Result<()> {
        match self.platform.kind() {
            PlatformKind::X11 => Ok(()),
            PlatformKind::Mac | PlatformKind::Wayland => {
                refuse_secure_focus(self.platform.focus_security(app.target))
            }
        }
    }

    fn click(
        &self,
        app: &mut AppState<P::Element>,
        target: &TargetArg,
        button: MouseButton,
        count: u32,
    ) -> Result<()> {
        let single_left = button == MouseButton::Left && count == 1;
        let index = match target {
            TargetArg::Index(index) => IndexArg::Valid(*index),
            TargetArg::Point(point) => {
                let point = self.image_point(app, point)?;
                return self.platform.click(app.target, point, button, count);
            }
            TargetArg::Invalid(type_name) => return Err(invalid_target(type_name)),
        };
        if let Some(actions) = self.platform.element_actions() {
            let (position, element, handle) = self.element(app, &index)?;
            if single_left {
                if let Some(fields) = actions.field_focus() {
                    if fields.is_text_field(&handle) {
                        // Clicking into a field focuses it; its default
                        // action (activate = Enter) never stands in for that.
                        if !fields.grab_focus(&handle) {
                            let center = self.element_point(app, &index)?;
                            self.platform
                                .click(app.target, center, MouseButton::Left, 1)?;
                            if !fields.wait_focused(&handle) {
                                return Err(ComputerUseError::new(
                                    ErrorCode::InjectionFailed,
                                    format!(
                                        "element [{position}] did not take keyboard focus; \
                                         re-observe and retry, or use set_value() for an \
                                         editable field"
                                    ),
                                )
                                .with_details(json!({"element_index": position})));
                            }
                        }
                        return Ok(());
                    }
                }
                if let Some(press) = actions.default_action(&element.actions) {
                    return actions.perform(&handle, &press);
                }
            }
        }
        let point = self.element_point(app, &index)?;
        self.platform.click(app.target, point, button, count)
    }

    fn set_value(&self, app: &AppState<P::Element>, index: &IndexArg, value: &str) -> Result<()> {
        if self.platform.kind() == PlatformKind::X11 {
            return Err(x11_gap(
                "set_value",
                "set it with click and type_text instead",
            ));
        }
        let (position, element, handle) = self.element(app, index)?;
        let actions = self.element_actions()?;
        refuse_secure_write(
            position,
            element.is_secure(),
            actions.live_security(&handle),
        )?;
        if !actions.is_settable(&handle) {
            return Err(unsupported(format!(
                "element {position} does not accept value writes; it is not editable text"
            ))
            .with_details(json!({"element_index": position})));
        }
        actions.set_value(&handle, value)
    }

    fn select_text(
        &self,
        app: &AppState<P::Element>,
        index: &IndexArg,
        text: &str,
        prefix: Option<&str>,
        suffix: Option<&str>,
    ) -> Result<()> {
        if self.platform.kind() == PlatformKind::X11 {
            return Err(x11_gap("select_text", "select by dragging instead"));
        }
        let (position, element, handle) = self.element(app, index)?;
        let actions = self.element_actions()?;
        refuse_secure_write(
            position,
            element.is_secure(),
            actions.live_security(&handle),
        )?;
        let Some(value) = actions.current_value(&handle) else {
            return Err(
                unsupported(format!("element {position} has no readable text to search"))
                    .with_details(json!({"element_index": position})),
            );
        };
        let chars =
            |text: Option<&str>| -> Vec<char> { text.unwrap_or_default().chars().collect() };
        let needle = chars(Some(text));
        let starts = occurrences(
            &chars(Some(&value)),
            &needle,
            &chars(prefix),
            &chars(suffix),
        );
        match starts[..] {
            [] => Err(ComputerUseError::new(
                ErrorCode::ElementStale,
                format!(
                    "{} is not in the element's current text; re-observe with get_ax_state()",
                    repr_str(text)
                ),
            )
            .with_details(json!({"element_index": position}))),
            [start] => actions.select_range(&handle, start, needle.len()),
            [..] => Err(unsupported(format!(
                "{} occurs {} times; disambiguate it with prefix and suffix",
                repr_str(text),
                starts.len()
            ))
            .with_details(json!({"element_index": position, "occurrences": starts.len()}))),
        }
    }

    /// Paste through the clipboard with cmd+v, restoring it afterwards
    /// unless it no longer holds the payload (a copy made meanwhile wins).
    fn paste(&self, app: &AppState<P::Element>, text: &str, format: PasteFormat) -> Result<()> {
        let clipboard = match self.platform.kind() {
            PlatformKind::X11 => {
                return Err(x11_gap("paste", "type the text with type_text instead"))
            }
            PlatformKind::Wayland => {
                return Err(wayland_gap(
                    "paste",
                    "the clipboard save/restore transaction is macOS-only; set the text with \
                     set_value or type it with type_text instead",
                ));
            }
            PlatformKind::Mac => self
                .platform
                .clipboard()
                .ok_or_else(|| unsupported("the clipboard is not available on this backend"))?,
        };
        self.refuse_secure_focus(app)?;
        let Some(saved) = clipboard.save() else {
            return Err(transport(
                "could not snapshot the clipboard; refusing to paste and risk the user's clipboard",
            )
            .with_details(json!({})));
        };
        let _transaction = PASTE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let mut wrote = false;
        let mut change_count = None;
        let result = (|| {
            clipboard.write(text, format).map_err(|reason| {
                transport(format!(
                    "could not write the paste payload: {}",
                    head(&reason, ERROR_LIMIT)
                ))
                .with_details(json!({}))
            })?;
            wrote = true;
            if !clipboard.holds(text) {
                // A concurrent copy displaced the payload: never paste it,
                // never restore over it.
                return Err(transport(
                    "the clipboard changed during the paste; the payload was not pasted",
                )
                .with_details(json!({})));
            }
            change_count = clipboard.change_count();
            self.platform
                .press_key(app.target, &parse_chord("cmd+v")?)?;
            std::thread::sleep(self.context.timing.paste_settle);
            Ok(())
        })();
        // A failed write leaves the cleared pasteboard behind: restore it. A
        // written payload restores only while the clipboard is unchanged
        // (the change count covers every type; without it, the payload
        // text is compared, and an unverifiable clipboard counts as changed).
        let unchanged = || match (clipboard.change_count(), change_count) {
            (Some(current), Some(written)) => current == written,
            _ => clipboard.holds(text),
        };
        if !wrote || unchanged() {
            clipboard.restore(&saved);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrences_respect_prefix_suffix_and_stop_at_two() {
        let chars = |text: &str| text.chars().collect::<Vec<_>>();
        assert_eq!(occurrences(&chars("query"), &chars("ue"), &[], &[]), [1]);
        assert_eq!(
            occurrences(&chars("query"), &chars("er"), &chars("u"), &chars("y")),
            [2]
        );
        assert_eq!(
            occurrences(&chars("ab cd ab ab"), &chars("ab"), &[], &[]),
            [0, 6]
        );
        assert_eq!(occurrences(&chars("aaa"), &chars("aa"), &[], &[]), [0, 1]);
        assert_eq!(
            occurrences(&chars("hé wörld"), &chars("wö"), &chars(" "), &[]),
            [3]
        );
        assert!(occurrences(&chars("ab"), &chars("abc"), &[], &[]).is_empty());
    }

    #[test]
    fn vision_boxes_become_sorted_top_left_regions() {
        let raw = |text: &str, bbox| RecognizedText {
            text: text.to_string(),
            confidence: 0.9,
            bbox,
        };
        let regions = text_regions(vec![
            raw("low", (0.5, 0.1, 0.2, 0.1)),
            raw("high-right", (0.6, 0.8, 0.1, 0.1)),
            raw("high-left", (0.1, 0.8, 0.1, 0.1)),
            raw("clamped", (-0.2, 1.2, 0.3, 0.1)),
        ]);
        let order: Vec<&str> = regions.iter().map(|(text, ..)| text.as_str()).collect();
        assert_eq!(order, ["clamped", "high-left", "high-right", "low"]);
        let close = |actual: [f64; 4], expected: [f64; 4]| {
            actual
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 1e-12)
        };
        assert!(
            close(regions[0].2, [0.0, 0.0, 0.3, 0.1]),
            "{:?}",
            regions[0]
        );
        assert!(
            close(regions[3].2, [0.5, 0.8, 0.2, 0.1]),
            "{:?}",
            regions[3]
        );
        // Truncation follows the sort: the 400 kept are the top-most ones.
        #[allow(clippy::cast_precision_loss)] // small indices
        let many: Vec<RecognizedText> = (0..450)
            .map(|index| {
                raw(
                    &index.to_string(),
                    (0.0, f64::from(index) / 1000.0, 0.0, 0.0),
                )
            })
            .collect();
        let kept = text_regions(many);
        assert_eq!(kept.len(), 400);
        assert_eq!((kept[0].0.as_str(), kept[399].0.as_str()), ("449", "50"));
        assert!(text_regions(Vec::new()).is_empty());
    }
}
