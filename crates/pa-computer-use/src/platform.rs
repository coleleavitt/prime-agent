//! The seam between the platform-independent session logic and one OS backend.
//!
//! [`Platform`] is what a backend exposes: discovery, the lock and permission
//! probes, observation, input, capture, and the optional capabilities only
//! some backends have (element actions, focus control, the clipboard, OCR).
//! Everything above it — the allowlist gate, the guard, the secure-field
//! refusals, coordinate mapping, diffing, telemetry — lives in
//! [`crate::session`] and runs identically on every backend, which is what
//! lets the test doubles below the seam exercise it on any host.

use std::time::Duration;

use crate::element::{Observation, Pair, Rect};
use crate::error::Result;
use crate::keymap::ParsedChord;
use crate::permissions::PermissionReport;
use crate::secure::Security;
use crate::spec::AppSpec;

#[cfg(any(target_os = "linux", all(test, unix)))]
mod logind;
#[cfg(any(target_os = "linux", all(test, unix)))]
pub(crate) mod x11;

/// The backend families, as `get_state()["platform"]` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformKind {
    Mac,
    /// The X11 backend (`"linux"` on the wire, its name since it shipped first).
    X11,
    /// The Wayland backend for niri.
    Wayland,
}

impl PlatformKind {
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            PlatformKind::Mac => "mac",
            PlatformKind::X11 => "linux",
            PlatformKind::Wayland => "wayland",
        }
    }
}

/// What a bound app's handle addresses: the pid on macOS, the window id on
/// X11 and Wayland (reported to the model as `App.pid` either way).
pub(crate) type Target = i64;

/// A cheap live identity of the focused window, compared for equality only.
pub(crate) type Fingerprint = Vec<Option<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// One running app as the macOS workspace reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningApp {
    pub bundle_id: String,
    pub name: String,
    pub pid: i64,
    pub path: Option<String>,
}

/// One window an app spec resolved to on a window-based backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowCandidate {
    /// The `WM_CLASS` on X11, the Wayland `app_id`: the allowlist's key.
    pub app_id: String,
    pub window_id: i64,
}

/// One `list_apps()` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppEntry {
    pub id: String,
    pub name: String,
}

/// The macOS discovery model: running apps, Spotlight, launching.
pub(crate) trait Workspace: Send + Sync {
    /// The running regular apps.
    fn running_apps(&self) -> Result<Vec<RunningApp>>;
    /// An installed app's bundle id by display name, never launching it;
    /// `AMBIGUOUS_APP` when several installed bundles share the name.
    fn bundle_for_name(&self, name: &str) -> Result<Option<String>>;
    /// The bundle id an app bundle directory declares, best-effort.
    fn bundle_id_for_path(&self, path: &str) -> Option<String>;
    /// Launch one already-gated bundle id in the background and wait for it.
    fn launch(&self, bundle_id: &str) -> Result<RunningApp>;
}

/// The window-based discovery model (X11, Wayland): binding attaches to a
/// running window and never launches anything.
pub(crate) trait WindowDirectory: Send + Sync {
    fn list_apps(&self) -> Result<Vec<AppEntry>>;
    /// The windows `spec` names, the one to bind first.
    fn resolve(&self, spec: &AppSpec) -> Result<Vec<WindowCandidate>>;
    /// Whether `window_id` still is one of `app_id`'s windows.
    fn owns(&self, window_id: i64, app_id: &str) -> Result<bool>;
}

/// How a backend discovers and binds apps.
pub(crate) enum Discovery<'a> {
    // Only the macOS backend (and the test double) discovers this way.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Workspace(&'a dyn Workspace),
    Windows(&'a dyn WindowDirectory),
}

/// What one screenshot request captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureRequest {
    /// macOS: the window by its `CGWindowID` (`screencapture -l`), else the
    /// screen region (`-R`) — the documented fallback.
    Region {
        origin: (i64, i64),
        size: (i64, i64),
        window_id: Option<i64>,
    },
    /// X11 and Wayland: the bound window, found by the backend.
    Window(Target),
}

/// One captured PNG.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Captured {
    pub path: String,
    /// The PNG's own pixel size (2x the logical size on a Retina/2x output).
    pub width: u32,
    pub height: u32,
    /// The logical rect the capture covers, when the backend computed it
    /// (Wayland); screenshot points scale back through it.
    pub logical_rect: Option<Rect>,
}

/// Accessibility actions on observed elements (macOS AX, AT-SPI).
pub(crate) trait ElementActions<E>: Send + Sync {
    /// The element's default activation (macOS `AXPress`; the AT-SPI
    /// click/press/activate/... family), when it exposes one.
    fn default_action(&self, actions: &[String]) -> Option<String>;
    fn perform(&self, element: &E, action: &str) -> Result<()>;
    fn is_settable(&self, element: &E) -> bool;
    /// The element's full current text, when readable.
    fn current_value(&self, element: &E) -> Option<String>;
    fn set_value(&self, element: &E, value: &str) -> Result<()>;
    /// Select `[location, location + length)` in code points.
    fn select_range(&self, element: &E, location: usize, length: usize) -> Result<()>;
    /// The live element's secure state.
    fn live_security(&self, element: &E) -> Security;
    /// Text-field focusing (Wayland): a field's default action never
    /// stands in for clicking into it.
    fn field_focus(&self) -> Option<&dyn FieldFocus<E>> {
        None
    }
}

/// Focusing text fields where clicking one must focus it.
pub(crate) trait FieldFocus<E>: Send + Sync {
    fn is_text_field(&self, element: &E) -> bool;
    /// Ask the toolkit to focus the field; `false` sends the caller to a
    /// real pointer click.
    fn grab_focus(&self, element: &E) -> bool;
    /// Wait (bounded) for the field to report focus.
    fn wait_focused(&self, element: &E) -> bool;
}

/// Bringing the bound app forward.
pub(crate) trait FocusControl: Send + Sync {
    fn activate(&self, target: Target) -> Result<()>;
    fn is_frontmost(&self, target: Target) -> Result<bool>;
}

/// One paste payload format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteFormat {
    Text,
    Markdown,
    Html,
}

/// A snapshot of every pasteboard type's data, for the restore.
pub(crate) type ClipboardSnapshot = Vec<(String, Vec<u8>)>;

/// The system clipboard (macOS).
pub(crate) trait Clipboard: Send + Sync {
    /// Every type's data; `None` is a failed read (paste then aborts).
    fn save(&self) -> Option<ClipboardSnapshot>;
    /// Write the payload; the error text lands in the paste's failure.
    fn write(&self, text: &str, format: PasteFormat) -> std::result::Result<(), String>;
    /// Whether the clipboard's plain text is exactly `text`.
    fn holds(&self, text: &str) -> bool;
    fn change_count(&self) -> Option<i64>;
    /// Restore every saved type, best-effort, one type never blocking the rest.
    fn restore(&self, snapshot: &ClipboardSnapshot);
}

/// One recognized text region in Vision's image-normalized, bottom-left
/// coordinates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecognizedText {
    pub text: String,
    pub confidence: f64,
    /// (x, y, width, height), y from the bottom.
    pub bbox: (f64, f64, f64, f64),
}

/// Text recognition over a captured window image (macOS Vision).
pub(crate) trait TextRecognizer: Send + Sync {
    fn recognize(&self, path: &str) -> std::result::Result<Vec<RecognizedText>, String>;
}

/// One OS backend. Implementations are synchronous and may block (AX and
/// AT-SPI calls, subprocesses); the host runs them off the async runtime.
pub(crate) trait Platform: Send + Sync + 'static {
    /// The live handle of one observed element.
    type Element: Clone + Send + Sync + 'static;

    fn kind(&self) -> PlatformKind;
    fn discovery(&self) -> Discovery<'_>;
    /// The session lock, failing closed: an unreadable state is locked.
    fn screen_locked(&self) -> bool;
    fn permissions(&self) -> PermissionReport;

    /// Snapshot the target's focused window.
    fn observe(&self, target: Target) -> Result<Observation<Self::Element>>;
    /// One live element's current (role, title), for freshness checks.
    fn live_fingerprint(&self, element: &Self::Element)
        -> Result<(Option<String>, Option<String>)>;
    /// The settle poll's read; `None` when the window cannot be read.
    fn window_fingerprint(&self, target: Target, budget: Duration) -> Option<Fingerprint>;
    /// The live focused element's secure state (not consulted on X11,
    /// which has no secure-input role).
    fn focus_security(&self, target: Target) -> Security;

    fn click(&self, target: Target, point: Pair, button: MouseButton, count: u32) -> Result<()>;
    fn drag(&self, target: Target, from: Pair, to: Pair) -> Result<()>;
    /// Scroll `pages` pages at `point` (input space, like [`Platform::click`]).
    fn scroll(
        &self,
        target: Target,
        direction: ScrollDirection,
        pages: u32,
        point: Pair,
    ) -> Result<()>;
    fn press_key(&self, target: Target, chord: &ParsedChord) -> Result<()>;
    fn type_text(&self, target: Target, text: &str) -> Result<()>;
    fn capture(&self, request: CaptureRequest) -> Result<Captured>;

    fn element_actions(&self) -> Option<&dyn ElementActions<Self::Element>> {
        None
    }
    fn focus_control(&self) -> Option<&dyn FocusControl> {
        None
    }
    fn clipboard(&self) -> Option<&dyn Clipboard> {
        None
    }
    fn text_recognizer(&self) -> Option<&dyn TextRecognizer> {
        None
    }
}
