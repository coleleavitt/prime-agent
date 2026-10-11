//! The macOS backend: AX observation and element actions, `CGEvent` input
//! posted to the app's pid, `screencapture` captures, `NSWorkspace`
//! discovery, the pasteboard and Vision text recognition.
//!
//! The rules live in this module and its children, platform-independent and
//! tested on any host over test doubles: [`ax`] holds the accessibility
//! reads (over the [`ax::Ax`] transport), [`events`] the event sequences, and
//! this file the workspace, launch and capture flows (subprocesses through
//! [`Tools`], like the Python skill). The only code touching the OS is
//! `sys`, compiled on macOS alone: the one module of the crate allowed
//! `unsafe` (objc2 and CoreFoundation calls), each block justified.

pub(crate) mod ax;
pub(crate) mod events;
pub(crate) mod pasteboard;

#[cfg(target_os = "macos")]
mod sys;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;

use self::ax::{Accessibility, Ax};
use self::events::MacEvent;
use crate::capture::CaptureDir;
use crate::element::{Observation, Pair, Rect};
use crate::error::{
    ComputerUseError,
    ERROR_LIMIT,
    ErrorCode,
    Result,
    head,
    injection_failed,
    invalid,
    not_running,
    transport,
};
use crate::keymap::ParsedChord;
use crate::permissions::{PermissionReport, PermissionState};
use crate::platform::{
    CaptureRequest,
    Captured,
    Clipboard,
    Discovery,
    ElementActions,
    Fingerprint,
    FocusControl,
    MouseButton,
    Platform,
    PlatformKind,
    RunningApp,
    ScrollDirection,
    Target,
    TextRecognizer,
    Workspace,
};
use crate::process::{RunError, TOOL_TIMEOUT, Tools, run_tool};
use crate::pyfmt::repr_str;
use crate::secure::Security;
use crate::session::expand_user;
use crate::spec::is_blank;

const SCREENCAPTURE: &str = "/usr/sbin/screencapture";
const MDFIND_TIMEOUT: Duration = Duration::from_secs(5);
const MDFIND_RESULT_CAP: usize = 5;
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const APPEAR_TIMEOUT: Duration = Duration::from_secs(15);
const APPEAR_POLL: Duration = Duration::from_millis(250);

/// One running application as `NSWorkspace` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceApp {
    pub bundle_id: Option<String>,
    pub name: Option<String>,
    pub pid: i64,
    pub path: Option<String>,
    /// `NSApplicationActivationPolicyRegular`: a Dock app with windows.
    pub regular: bool,
}

/// An event that could not be created or posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PostError(pub String);

/// `AppKit`, `CoreGraphics` and the session: everything outside AX.
pub(crate) trait Desktop: Clipboard + TextRecognizer + Send + Sync {
    fn running_applications(&self) -> Vec<WorkspaceApp>;
    fn frontmost_pid(&self) -> Option<i64>;
    /// Make the app with `pid` key (ignoring other apps); `false` when no
    /// running app has the pid.
    fn activate(&self, pid: i64) -> bool;
    /// The `CFBundleIdentifier` of the app bundle at `bundle_dir`.
    fn bundle_identifier(&self, bundle_dir: &Path) -> Option<String>;
    /// The session dictionary's `CGSSessionScreenIsLocked` flag; `None`
    /// when there is no readable session dictionary.
    fn session_locked(&self) -> Option<bool>;
    /// The Accessibility grant, read without prompting.
    fn accessibility_trusted(&self) -> Option<bool>;
    /// The Screen Recording preflight (never the prompting request call).
    fn screen_capture_allowed(&self) -> Option<bool>;
    /// The on-screen windows' `CGWindowID` and bounds, front to back.
    fn on_screen_windows(&self) -> Option<Vec<(i64, Rect)>>;
    /// Post `events` to the process `pid`, in order.
    ///
    /// # Errors
    ///
    /// The first event that could not be created or posted.
    fn post(&self, pid: i64, events: &[MacEvent]) -> std::result::Result<(), PostError>;
}

/// Python's `os.path.abspath` for a `which` result.
fn absolute_path(path: &str) -> String {
    std::path::absolute(path).map_or_else(
        |_| path.to_string(),
        |path| path.to_string_lossy().into_owned(),
    )
}

/// Escape the Spotlight query metacharacters of a literal display name.
fn escape_spotlight(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('*', "\\*")
        .replace('?', "\\?")
}

/// The macOS backend over its seams.
pub(crate) struct MacPlatform<T: Tools, X: Ax, D: Desktop> {
    tools: T,
    accessibility: Accessibility<X>,
    desktop: D,
    capture: CaptureDir,
    /// How long a launched app has to show up in the running apps.
    appear_timeout: Duration,
    appear_poll: Duration,
}

impl<T: Tools, X: Ax, D: Desktop> MacPlatform<T, X, D> {
    pub(crate) fn new(tools: T, ax: X, desktop: D, capture: CaptureDir) -> Self {
        Self {
            tools,
            accessibility: Accessibility { ax },
            desktop,
            capture,
            appear_timeout: APPEAR_TIMEOUT,
            appear_poll: APPEAR_POLL,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_appear_timeout(mut self, timeout: Duration, poll: Duration) -> Self {
        self.appear_timeout = timeout;
        self.appear_poll = poll;
        self
    }

    /// A window's bounds from the window server, matched by its `CGWindowID`.
    fn server_rect(&self, window_id: i64) -> Option<Rect> {
        self.desktop
            .on_screen_windows()?
            .into_iter()
            .find_map(|(id, rect)| (id == window_id).then_some(rect))
    }

    fn post(&self, action: &str, pid: Target, events: &[MacEvent]) -> Result<()> {
        self.desktop.post(pid, events).map_err(|PostError(reason)| {
            injection_failed(format!("{action} failed: {}", head(&reason, ERROR_LIMIT)))
                .with_details(json!({"pid": pid}))
        })
    }

    /// The window (`-l`, scoped to it alone) or else the screen region
    /// (`-R`, whatever is on screen there) into a fresh private PNG.
    fn screencapture(
        &self,
        (x, y): (i64, i64),
        (width, height): (i64, i64),
        window_id: Option<i64>,
    ) -> Result<Captured> {
        if width < 1 || height < 1 {
            return Err(
                invalid("size must be a (width, height) pair with positive values")
                    .with_details(json!({"size": [width, height]})),
            );
        }
        let dir = self.capture.open()?;
        let binary = if self.tools.is_file(SCREENCAPTURE) {
            SCREENCAPTURE.to_string()
        } else {
            match self.tools.which("screencapture") {
                Some(found) => absolute_path(&found),
                None => return Err(transport("screencapture is not available on this system")),
            }
        };
        let (name, path) = dir.new_target()?;
        let path = path.to_string_lossy().into_owned();
        let scope = match window_id {
            None => ["-R".to_string(), format!("{x},{y},{width},{height}")],
            Some(id) => ["-l".to_string(), id.to_string()],
        };
        let argv = [
            vec![binary, "-x".to_string(), "-o".to_string()],
            scope.to_vec(),
            vec![path.clone()],
        ]
        .concat();
        let output = run_tool(&self.tools, &argv, TOOL_TIMEOUT)?;
        if !output.success() {
            let reason = output.capped();
            if reason.to_lowercase().contains("window") {
                return Err(not_running(format!(
                    "screencapture could not find the window: {reason}"
                )));
            }
            return Err(transport(format!(
                "screencapture failed with exit code {}: {reason}",
                output.code
            )));
        }
        dir.make_private(&name);
        let (png_width, png_height) = dir.png_dimensions(&name)?;
        let far = |png: u32, wanted: i64| i64::from(png) > wanted.saturating_mul(2);
        if window_id.is_none() && (far(png_width, width) || far(png_height, height)) {
            return Err(transport(format!(
                "captured image is {png_width}x{png_height}, far from the requested {width}x{height} region"
            )));
        }
        dir.sweep(Some(&name));
        Ok(Captured {
            path,
            width: png_width,
            height: png_height,
            logical_rect: None,
        })
    }

    fn running(&self) -> Vec<RunningApp> {
        self.desktop
            .running_applications()
            .into_iter()
            .filter_map(|app| {
                let bundle_id = app.bundle_id.filter(|id| !id.is_empty())?;
                app.regular.then(|| RunningApp {
                    name: app
                        .name
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| bundle_id.clone()),
                    bundle_id,
                    pid: app.pid,
                    path: app.path.filter(|path| !path.is_empty()),
                })
            })
            .collect()
    }
}

impl<T: Tools, X: Ax, D: Desktop> Workspace for MacPlatform<T, X, D> {
    fn running_apps(&self) -> Result<Vec<RunningApp>> {
        Ok(self.running())
    }

    /// Spotlight (`mdfind`) by display name, never launching anything; the
    /// first five hits' bundle ids. An unrunnable or failing query resolves
    /// nothing.
    fn bundle_for_name(&self, name: &str) -> Result<Option<String>> {
        if is_blank(name) {
            return Ok(None);
        }
        let query = format!(
            "kMDItemContentTypeTree == \"com.apple.application\" && kMDItemDisplayName == \"{}\"",
            escape_spotlight(name)
        );
        let output = match self
            .tools
            .run(&["mdfind".to_string(), query], MDFIND_TIMEOUT)
        {
            Ok(output) if output.success() => output,
            Ok(_) | Err(_) => return Ok(None),
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut bundle_ids: Vec<String> = Vec::new();
        for path in stdout.lines().take(MDFIND_RESULT_CAP) {
            let Some(bundle_id) = self.desktop.bundle_identifier(&expand_user(path.trim())) else {
                continue;
            };
            if bundle_ids.contains(&bundle_id) {
                continue;
            }
            bundle_ids.push(bundle_id);
            if bundle_ids.len() > 1 {
                return Err(ComputerUseError::new(
                    ErrorCode::AmbiguousApp,
                    format!(
                        "the name {} matches several installed apps ({}); call get_app with the \
                         bundle_id of the one you want",
                        repr_str(name),
                        bundle_ids.join(", ")
                    ),
                )
                .with_details(json!({"bundle_ids": bundle_ids})));
            }
        }
        Ok(bundle_ids.into_iter().next())
    }

    fn bundle_id_for_path(&self, path: &str) -> Option<String> {
        self.desktop.bundle_identifier(&expand_user(path))
    }

    /// `open -g -b` (in the background: binding never steals the user's
    /// screen), then wait for the app to show up.
    fn launch(&self, bundle_id: &str) -> Result<RunningApp> {
        let command = ["open", "-g", "-b", bundle_id].map(ToString::to_string);
        let failed = |message: String| ComputerUseError::new(ErrorCode::AppLaunchFailed, message);
        let output = self
            .tools
            .run(&command, OPEN_TIMEOUT)
            .map_err(|error| match error {
                RunError::TimedOut => failed(format!(
                    "open timed out after {} seconds for {bundle_id}",
                    OPEN_TIMEOUT.as_secs()
                )),
                RunError::Unavailable(reason) => failed(format!(
                    "open is not available: {}",
                    head(&reason, ERROR_LIMIT)
                )),
            })?;
        if !output.success() {
            return Err(
                failed(format!("open failed for {bundle_id}: {}", output.capped()))
                    .with_details(json!({"command": head(&command.join(" "), ERROR_LIMIT)})),
            );
        }
        let deadline = Instant::now() + self.appear_timeout;
        while Instant::now() < deadline {
            if let Some(app) = self
                .running()
                .into_iter()
                .find(|app| app.bundle_id == bundle_id)
            {
                return Ok(app);
            }
            std::thread::sleep(self.appear_poll);
        }
        let spec = format!("{{'bundle_id': {}}}", repr_str(bundle_id));
        Err(not_running(format!(
            "{bundle_id} did not start within {} seconds; call get_app again once it is running",
            self.appear_timeout.as_secs()
        ))
        .with_details(json!({"spec": head(&spec, ERROR_LIMIT)})))
    }
}

impl<T: Tools, X: Ax, D: Desktop> ElementActions<X::Node> for MacPlatform<T, X, D> {
    fn default_action(&self, actions: &[String]) -> Option<String> {
        actions
            .iter()
            .any(|action| action == "AXPress")
            .then(|| "AXPress".to_string())
    }

    fn perform(&self, element: &X::Node, action: &str) -> Result<()> {
        self.accessibility.perform(element, action)
    }

    fn is_settable(&self, element: &X::Node) -> bool {
        self.accessibility.is_settable(element, "AXValue")
    }

    fn current_value(&self, element: &X::Node) -> Option<String> {
        self.accessibility.current_value(element)
    }

    fn set_value(&self, element: &X::Node, value: &str) -> Result<()> {
        self.accessibility.set_value(element, value)
    }

    fn select_range(&self, element: &X::Node, location: usize, length: usize) -> Result<()> {
        self.accessibility.select_range(element, location, length)
    }

    fn live_security(&self, element: &X::Node) -> Security {
        Security::from_probe(self.accessibility.live_is_secure(element))
    }
}

impl<T: Tools, X: Ax, D: Desktop> FocusControl for MacPlatform<T, X, D> {
    fn activate(&self, target: Target) -> Result<()> {
        if self.desktop.activate(target) {
            return Ok(());
        }
        Err(
            not_running("the app is no longer running; bind it again with get_app()")
                .with_details(json!({"pid": target})),
        )
    }

    fn is_frontmost(&self, target: Target) -> Result<bool> {
        Ok(self.desktop.frontmost_pid() == Some(target))
    }
}

impl<T, X, D> Platform for MacPlatform<T, X, D>
where
    T: Tools + 'static,
    X: Ax + 'static,
    D: Desktop + 'static,
{
    type Element = X::Node;

    fn kind(&self) -> PlatformKind {
        PlatformKind::Mac
    }

    fn discovery(&self) -> Discovery<'_> {
        Discovery::Workspace(self)
    }

    /// An unreadable session dictionary reads as locked (fail closed).
    fn screen_locked(&self) -> bool {
        self.desktop.session_locked().unwrap_or(true)
    }

    fn permissions(&self) -> PermissionReport {
        PermissionReport::Grants {
            accessibility: PermissionState::from_probe(self.desktop.accessibility_trusted()),
            screen_recording: PermissionState::from_probe(self.desktop.screen_capture_allowed()),
        }
    }

    fn observe(&self, target: Target) -> Result<Observation<X::Node>> {
        Ok(self
            .accessibility
            .observe(target, |id| self.server_rect(id)))
    }

    fn live_fingerprint(&self, element: &X::Node) -> Result<(Option<String>, Option<String>)> {
        Ok(self.accessibility.live_fingerprint(element))
    }

    fn window_fingerprint(&self, target: Target, budget: Duration) -> Option<Fingerprint> {
        self.accessibility.window_fingerprint(target, Some(budget))
    }

    fn focus_security(&self, target: Target) -> Security {
        Security::from_probe(self.accessibility.focused_is_secure(target))
    }

    fn click(&self, target: Target, point: Pair, button: MouseButton, count: u32) -> Result<()> {
        self.post("click", target, &events::click(point, button, count))
    }

    fn drag(&self, target: Target, from: Pair, to: Pair) -> Result<()> {
        self.post("drag", target, &events::drag(from, to))
    }

    fn scroll(
        &self,
        target: Target,
        direction: ScrollDirection,
        pages: u32,
        point: Pair,
    ) -> Result<()> {
        self.post("scroll", target, &[events::scroll(direction, pages, point)])
    }

    fn press_key(&self, target: Target, chord: &ParsedChord) -> Result<()> {
        self.post("press_key", target, &events::chord(chord)?)
    }

    fn type_text(&self, target: Target, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.post("type_text", target, &events::text(text))
    }

    fn capture(&self, request: CaptureRequest) -> Result<Captured> {
        match request {
            CaptureRequest::Region {
                origin,
                size,
                window_id,
            } => self.screencapture(origin, size, window_id),
            CaptureRequest::Window(_) => Err(invalid(
                "the macOS backend captures by window id or screen region",
            )),
        }
    }

    fn element_actions(&self) -> Option<&dyn ElementActions<X::Node>> {
        Some(self)
    }

    fn focus_control(&self) -> Option<&dyn FocusControl> {
        Some(self)
    }

    fn clipboard(&self) -> Option<&dyn Clipboard> {
        Some(&self.desktop)
    }

    fn text_recognizer(&self) -> Option<&dyn TextRecognizer> {
        Some(&self.desktop)
    }
}

/// The host's macOS backend.
#[cfg(target_os = "macos")]
pub(crate) mod native {
    use std::path::Path;

    use super::MacPlatform;
    use super::sys::{SysAx, SysDesktop};
    use crate::capture::CaptureDir;
    use crate::process::SystemTools;

    pub(crate) type NativeMac = MacPlatform<SystemTools, SysAx, SysDesktop>;

    pub(crate) fn platform(agent_dir: &Path) -> NativeMac {
        MacPlatform::new(
            SystemTools,
            SysAx,
            SysDesktop,
            CaptureDir::under_agent_dir(agent_dir),
        )
    }
}

#[cfg(test)]
mod fakes;
#[cfg(test)]
mod tests;
