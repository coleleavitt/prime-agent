//! The test double below the platform seam: one fully faked macOS-shaped
//! environment (the skill's `fakes.AppEnvironment`), with the platform kind
//! switchable so the X11- and Wayland-only App rules run on it too.
//!
//! Element handles are the observed [`Element`]s themselves; every input,
//! AX and clipboard call is recorded. No display, grant, app or framework
//! is touched.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::json;

use super::{HostContext, Session, Timing};
use crate::element::{flatten, Element, Observation, Pair, Rect};
use crate::error::{not_running, ComputerUseError, Result};
use crate::keymap::ParsedChord;
use crate::permissions::{PermissionReport, PermissionState};
use crate::platform::{
    CaptureRequest, Captured, Clipboard, ClipboardSnapshot, Discovery, ElementActions, Fingerprint,
    FocusControl, MouseButton, PasteFormat, Platform, PlatformKind, RecognizedText, RunningApp,
    ScrollDirection, Target, TextRecognizer, Workspace,
};
use crate::policy::Policy;
use crate::secure::Security;
use crate::telemetry::{TelemetryEvent, TelemetrySink};
use crate::testing::{small_tree, write_settings};

/// One recorded input, activation or capture call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Call {
    Click {
        pid: Target,
        point: Pair,
        button: MouseButton,
        count: u32,
    },
    Drag {
        pid: Target,
        start: Pair,
        end: Pair,
    },
    Scroll {
        pid: Target,
        direction: ScrollDirection,
        pages: u32,
        point: Pair,
    },
    PressKey {
        pid: Target,
        chord: ParsedChord,
    },
    TypeText {
        pid: Target,
        text: String,
    },
    Activate {
        pid: Target,
    },
    Screenshot(CaptureRequest),
}

/// One recorded AX element call (the element by title).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AxCall {
    Perform {
        title: Option<String>,
        action: String,
    },
    SetValue {
        title: Option<String>,
        value: String,
    },
    SelectRange {
        title: Option<String>,
        location: usize,
        length: usize,
    },
}

/// One recorded clipboard call.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ClipboardCall {
    Save,
    Write { format: PasteFormat, text: String },
    Restore(ClipboardSnapshot),
}

/// The faked environment's knobs and recordings.
#[allow(clippy::struct_excessive_bools)] // one switch per faked probe, as in the Python fake
pub(crate) struct FakeState {
    pub kind: PlatformKind,
    pub tree: Vec<Element>,
    pub window_title: Option<String>,
    pub window_rect: Option<Rect>,
    pub window_id: Option<i64>,
    pub focused_index: Option<usize>,
    pub truncated: bool,
    pub locked: bool,
    pub settable: bool,
    /// The live focused element's secure verdict; `None`: the read failed.
    pub secure_focus: Option<bool>,
    /// The live per-element secure verdict `set_value`/`select_text` read.
    pub live_secure: Option<bool>,
    /// When set, every live element reads as changed since the snapshot.
    pub drift: bool,
    /// Queued fingerprint reads, cycled; `None`: unreadable.
    pub fingerprints: Option<Vec<Fingerprint>>,
    pub fingerprint_reads: usize,
    pub accessibility: PermissionState,
    pub screen_recording: PermissionState,
    pub running: Vec<RunningApp>,
    pub running_error: Option<ComputerUseError>,
    pub launch_result: Option<RunningApp>,
    pub launch_calls: Vec<String>,
    /// Runs after a launch (a user revoking the app mid-bind).
    pub after_launch: Option<fn(&mut FakeState)>,
    pub bundle_for_name: Result<Option<String>>,
    pub bundle_for_path: Option<String>,
    pub frontmost: Option<Target>,
    pub screenshot: Captured,
    pub screenshot_error: Option<ComputerUseError>,
    /// Runs during a capture (a concurrent re-observe).
    pub during_capture: Option<fn(&mut FakeState)>,
    pub recognized: Vec<RecognizedText>,
    /// A recognition failure's reason.
    pub recognize_error: Option<String>,
    pub clipboard_saved: Option<ClipboardSnapshot>,
    pub clipboard_holds: bool,
    /// Served change counts, in order (the last repeats).
    pub change_counts: Vec<Option<i64>>,
    pub change_count_reads: usize,
    pub write_error: Option<String>,
    pub calls: Vec<Call>,
    pub ax_calls: Vec<AxCall>,
    pub clipboard_calls: Vec<ClipboardCall>,
}

pub(crate) const BUNDLE: &str = "com.example.app";
pub(crate) const NAME: &str = "Example";
pub(crate) const PID: Target = 4242;

impl Default for FakeState {
    fn default() -> Self {
        let app = RunningApp {
            bundle_id: BUNDLE.to_string(),
            name: NAME.to_string(),
            pid: PID,
            path: None,
        };
        Self {
            kind: PlatformKind::Mac,
            tree: small_tree(),
            window_title: Some("Main".to_string()),
            window_rect: Some(Rect::new(100.0, 50.0, 400.0, 300.0)),
            window_id: Some(4321),
            focused_index: None,
            truncated: false,
            locked: false,
            settable: true,
            secure_focus: Some(false),
            live_secure: Some(false),
            drift: false,
            fingerprints: None,
            fingerprint_reads: 0,
            accessibility: PermissionState::Ok,
            screen_recording: PermissionState::Ok,
            running: vec![app.clone()],
            running_error: None,
            launch_result: Some(app),
            launch_calls: Vec::new(),
            after_launch: None,
            bundle_for_name: Ok(None),
            bundle_for_path: None,
            frontmost: None,
            screenshot: Captured {
                path: "/tmp/computer-use-fake.png".to_string(),
                width: 400,
                height: 300,
                logical_rect: None,
            },
            screenshot_error: None,
            during_capture: None,
            recognized: Vec::new(),
            recognize_error: None,
            clipboard_saved: Some(vec![(
                "public.utf8-plain-text".to_string(),
                b"saved".to_vec(),
            )]),
            clipboard_holds: true,
            change_counts: vec![Some(3)],
            change_count_reads: 0,
            write_error: None,
            calls: Vec::new(),
            ax_calls: Vec::new(),
            clipboard_calls: Vec::new(),
        }
    }
}

/// The faked platform; clones share state.
#[derive(Clone, Default)]
pub(crate) struct FakePlatform {
    state: Arc<Mutex<FakeState>>,
}

impl FakePlatform {
    pub(crate) fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Workspace for FakePlatform {
    fn running_apps(&self) -> Result<Vec<RunningApp>> {
        let state = self.state();
        match &state.running_error {
            Some(error) => Err(error.clone()),
            None => Ok(state.running.clone()),
        }
    }

    fn bundle_for_name(&self, _name: &str) -> Result<Option<String>> {
        self.state().bundle_for_name.clone()
    }

    fn bundle_id_for_path(&self, _path: &str) -> Option<String> {
        self.state().bundle_for_path.clone()
    }

    fn launch(&self, bundle_id: &str) -> Result<RunningApp> {
        let mut state = self.state();
        state.launch_calls.push(bundle_id.to_string());
        let launched = state.launch_result.clone().expect("a canned launch result");
        if !state.running.contains(&launched) {
            state.running.push(launched.clone());
        }
        if let Some(after) = state.after_launch {
            after(&mut state);
        }
        Ok(launched)
    }
}

impl ElementActions<Element> for FakePlatform {
    fn default_action(&self, actions: &[String]) -> Option<String> {
        actions
            .iter()
            .any(|action| action == "AXPress")
            .then(|| "AXPress".to_string())
    }

    fn perform(&self, element: &Element, action: &str) -> Result<()> {
        self.state().ax_calls.push(AxCall::Perform {
            title: element.title.clone(),
            action: action.to_string(),
        });
        Ok(())
    }

    fn is_settable(&self, _element: &Element) -> bool {
        self.state().settable
    }

    fn current_value(&self, element: &Element) -> Option<String> {
        element.value.clone()
    }

    fn set_value(&self, element: &Element, value: &str) -> Result<()> {
        self.state().ax_calls.push(AxCall::SetValue {
            title: element.title.clone(),
            value: value.to_string(),
        });
        Ok(())
    }

    fn select_range(&self, element: &Element, location: usize, length: usize) -> Result<()> {
        self.state().ax_calls.push(AxCall::SelectRange {
            title: element.title.clone(),
            location,
            length,
        });
        Ok(())
    }

    fn live_security(&self, _element: &Element) -> Security {
        Security::from_probe(self.state().live_secure)
    }
}

impl FocusControl for FakePlatform {
    fn activate(&self, target: Target) -> Result<()> {
        let mut state = self.state();
        if !state.running.iter().any(|app| app.pid == target) {
            return Err(
                not_running("the app is no longer running; bind it again with get_app()")
                    .with_details(json!({"pid": target})),
            );
        }
        state.calls.push(Call::Activate { pid: target });
        Ok(())
    }

    fn is_frontmost(&self, target: Target) -> Result<bool> {
        Ok(self.state().frontmost == Some(target))
    }
}

impl Clipboard for FakePlatform {
    fn save(&self) -> Option<ClipboardSnapshot> {
        let mut state = self.state();
        let saved = state.clipboard_saved.clone();
        if saved.is_some() {
            state.clipboard_calls.push(ClipboardCall::Save);
        }
        saved
    }

    fn write(&self, text: &str, format: PasteFormat) -> std::result::Result<(), String> {
        let mut state = self.state();
        if let Some(error) = state.write_error.clone() {
            return Err(error);
        }
        state.clipboard_calls.push(ClipboardCall::Write {
            format,
            text: text.to_string(),
        });
        Ok(())
    }

    fn holds(&self, _text: &str) -> bool {
        self.state().clipboard_holds
    }

    fn change_count(&self) -> Option<i64> {
        let mut state = self.state();
        let index = state
            .change_count_reads
            .min(state.change_counts.len().saturating_sub(1));
        state.change_count_reads += 1;
        state.change_counts.get(index).copied().flatten()
    }

    fn restore(&self, snapshot: &ClipboardSnapshot) {
        self.state()
            .clipboard_calls
            .push(ClipboardCall::Restore(snapshot.clone()));
    }
}

impl TextRecognizer for FakePlatform {
    fn recognize(&self, _path: &str) -> std::result::Result<Vec<RecognizedText>, String> {
        let state = self.state();
        match &state.recognize_error {
            Some(reason) => Err(reason.clone()),
            None => Ok(state.recognized.clone()),
        }
    }
}

impl Platform for FakePlatform {
    type Element = Element;

    fn kind(&self) -> PlatformKind {
        self.state().kind
    }

    fn discovery(&self) -> Discovery<'_> {
        Discovery::Workspace(self)
    }

    fn screen_locked(&self) -> bool {
        self.state().locked
    }

    fn permissions(&self) -> PermissionReport {
        let state = self.state();
        PermissionReport::Grants {
            accessibility: state.accessibility,
            screen_recording: state.screen_recording,
        }
    }

    fn observe(&self, _target: Target) -> Result<Observation<Element>> {
        let state = self.state();
        Ok(Observation {
            window_title: state.window_title.clone(),
            refs: flatten(&state.tree).into_iter().cloned().collect(),
            tree: state.tree.clone(),
            window_rect: state.window_rect,
            focused_index: state.focused_index,
            window_id: state.window_id,
            truncated: state.truncated,
        })
    }

    fn live_fingerprint(&self, element: &Element) -> Result<(Option<String>, Option<String>)> {
        if self.state().drift {
            return Ok((
                Some("AXGhost".to_string()),
                Some("changed since the snapshot".to_string()),
            ));
        }
        Ok((element.role.clone(), element.title.clone()))
    }

    fn window_fingerprint(&self, _target: Target, _budget: Duration) -> Option<Fingerprint> {
        let mut state = self.state();
        state.fingerprint_reads += 1;
        let reads = state.fingerprint_reads;
        let values = state.fingerprints.as_ref()?;
        Some(values[(reads - 1) % values.len()].clone())
    }

    fn focus_security(&self, _target: Target) -> Security {
        Security::from_probe(self.state().secure_focus)
    }

    fn click(&self, target: Target, point: Pair, button: MouseButton, count: u32) -> Result<()> {
        self.state().calls.push(Call::Click {
            pid: target,
            point,
            button,
            count,
        });
        Ok(())
    }

    fn drag(&self, target: Target, from: Pair, to: Pair) -> Result<()> {
        self.state().calls.push(Call::Drag {
            pid: target,
            start: from,
            end: to,
        });
        Ok(())
    }

    fn scroll(
        &self,
        target: Target,
        direction: ScrollDirection,
        pages: u32,
        point: Pair,
    ) -> Result<()> {
        self.state().calls.push(Call::Scroll {
            pid: target,
            direction,
            pages,
            point,
        });
        Ok(())
    }

    fn press_key(&self, target: Target, chord: &ParsedChord) -> Result<()> {
        self.state().calls.push(Call::PressKey {
            pid: target,
            chord: chord.clone(),
        });
        Ok(())
    }

    fn type_text(&self, target: Target, text: &str) -> Result<()> {
        self.state().calls.push(Call::TypeText {
            pid: target,
            text: text.to_string(),
        });
        Ok(())
    }

    fn capture(&self, request: CaptureRequest) -> Result<Captured> {
        let mut state = self.state();
        state.calls.push(Call::Screenshot(request));
        if let Some(during) = state.during_capture {
            during(&mut state);
        }
        if let Some(error) = state.screenshot_error.clone() {
            return Err(error);
        }
        Ok(state.screenshot.clone())
    }

    fn element_actions(&self) -> Option<&dyn ElementActions<Element>> {
        Some(self)
    }

    fn focus_control(&self) -> Option<&dyn FocusControl> {
        Some(self)
    }

    fn clipboard(&self) -> Option<&dyn Clipboard> {
        Some(self)
    }

    fn text_recognizer(&self) -> Option<&dyn TextRecognizer> {
        Some(self)
    }
}

/// Records every telemetry event.
#[derive(Default)]
pub(crate) struct RecordingTelemetry {
    pub events: Mutex<Vec<TelemetryEvent>>,
}

impl TelemetrySink for RecordingTelemetry {
    fn track(&self, event: TelemetryEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

impl RecordingTelemetry {
    pub(crate) fn events(&self) -> Vec<TelemetryEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The settle and paste timings tests run with.
pub(crate) fn quick_timing() -> Timing {
    Timing {
        settle_poll: Duration::from_millis(5),
        settle_max: Duration::from_millis(50),
        paste_settle: Duration::ZERO,
    }
}

/// One faked session: the platform, its telemetry, and the agent dir
/// holding the settings fixture.
pub(crate) struct Env<P: Platform> {
    pub session: Session<P>,
    pub telemetry: Arc<RecordingTelemetry>,
    pub agent_dir: tempfile::TempDir,
}

impl<P: Platform> Env<P> {
    /// A session over `platform` allowing `allowed`.
    pub(crate) fn with_platform(platform: P, allowed: &[&str]) -> Self {
        Self::with_policy(platform, allowed, &[])
    }

    pub(crate) fn with_policy(platform: P, allowed: &[&str], blocked: &[&str]) -> Self {
        let agent_dir = tempfile::tempdir().expect("agent dir");
        let settings = agent_dir.path().join("settings");
        std::fs::create_dir_all(&settings).expect("settings dir");
        write_settings(&settings, allowed, blocked, &[], &[]);
        let telemetry = Arc::new(RecordingTelemetry::default());
        let context = HostContext::new(
            Policy::for_agent_dir(agent_dir.path()),
            Arc::clone(&telemetry) as Arc<dyn TelemetrySink>,
            quick_timing(),
        );
        Self {
            session: Session::new(platform, Arc::new(context)),
            telemetry,
            agent_dir,
        }
    }

    /// Rewrite the settings fixture (the user editing the allowlist).
    pub(crate) fn allow_only(&self, allowed: &[&str]) {
        write_settings(
            &self.agent_dir.path().join("settings"),
            allowed,
            &[],
            &[],
            &[],
        );
    }

    /// The `computer_use_action` events as (action, outcome) pairs.
    pub(crate) fn actions(&self) -> Vec<(&'static str, crate::telemetry::Outcome)> {
        self.telemetry
            .events()
            .into_iter()
            .filter_map(|event| match event {
                TelemetryEvent::Action {
                    action, outcome, ..
                } => Some((action, outcome)),
                TelemetryEvent::SessionStarted { .. } => None,
            })
            .collect()
    }
}

impl Env<FakePlatform> {
    /// The default faked macOS environment allowing the example app.
    pub(crate) fn new() -> Self {
        Self::with_platform(FakePlatform::default(), &[BUNDLE])
    }

    pub(crate) fn allowing(allowed: &[&str]) -> Self {
        Self::with_platform(FakePlatform::default(), allowed)
    }

    pub(crate) fn fake(&self) -> MutexGuard<'_, FakeState> {
        self.session.platform().state()
    }
}
