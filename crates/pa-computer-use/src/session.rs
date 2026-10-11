//! The platform-independent behaviour of the `computer_use` API.
//!
//! One [`Session`] serves one backend: `get_state`, `list_apps`,
//! `permissions_status`, binding (`get_app`) and every App method. It owns
//! the bound apps (their snapshots, element handles and screenshot scale)
//! and applies, on every backend alike, the allowlist gate, the locked-screen
//! and grant checks, the stale-binding guard, the secure-field refusals, the
//! coordinate mapping and the post-action settle.

mod actions;
mod app;
mod bind;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

pub(crate) use actions::{ActionArg, AppCall, TargetArg, TextArg};
pub(crate) use app::{AppState, IndexArg, PointArg};
#[cfg(any(target_os = "macos", all(test, unix)))]
pub(crate) use bind::expand_user;
use serde_json::{Value, json};

use crate::error::{ComputerUseError, ErrorCode, Result};
use crate::platform::{AppEntry, Discovery, Platform};
use crate::policy::Policy;
use crate::telemetry::{Outcome, TelemetryEvent, TelemetrySink};

/// The post-action settle and paste timings (shortened by tests).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    pub settle_poll: Duration,
    pub settle_max: Duration,
    pub paste_settle: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            settle_poll: Duration::from_millis(50),
            settle_max: Duration::from_millis(500),
            paste_settle: Duration::from_millis(100),
        }
    }
}

/// State shared by every backend's session of one host: the gate, the
/// telemetry sink, and the once-per-host flags of the Python module.
pub(crate) struct HostContext {
    pub policy: Policy,
    pub telemetry: Arc<dyn TelemetrySink>,
    pub timing: Timing,
    session_started: AtomicBool,
    instructions_shown: Mutex<HashSet<String>>,
}

impl HostContext {
    pub(crate) fn new(policy: Policy, telemetry: Arc<dyn TelemetrySink>, timing: Timing) -> Self {
        Self {
            policy,
            telemetry,
            timing,
            session_started: AtomicBool::new(false),
            instructions_shown: Mutex::new(HashSet::new()),
        }
    }

    /// `get_state(emit=True)`'s telemetry: the once-per-host session start,
    /// then the `get_state` action.
    pub(crate) fn emit_get_state(&self, platform: &'static str, started: Instant) {
        if !self.session_started.swap(true, Ordering::SeqCst) {
            self.telemetry
                .track(TelemetryEvent::SessionStarted { platform });
        }
        self.emit_action("get_state", started, Outcome::Ok);
    }

    pub(crate) fn emit_action(&self, action: &'static str, started: Instant, outcome: Outcome) {
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.telemetry.track(TelemetryEvent::Action {
            action,
            outcome,
            duration_ms,
        });
    }

    /// Whether this host shows `bundle_id`'s per-app instructions now: true
    /// once, the first time an observation of it has elements.
    fn first_instructions(&self, bundle_id: &str) -> bool {
        self.instructions_shown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(bundle_id.to_string())
    }
}

/// One bound app as `get_app` reports it to the client.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundApp {
    pub handle: u64,
    pub bundle_id: String,
    pub name: String,
    pub pid: i64,
    pub state: Option<String>,
}

impl BoundApp {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "handle": self.handle,
            "bundle_id": self.bundle_id,
            "name": self.name,
            "pid": self.pid,
            "state": self.state,
        })
    }
}

/// The most bindings one session keeps: a long session that re-binds many
/// times drops its oldest (an `App` object still holding an evicted handle
/// gets `APP_NOT_RUNNING` and re-binds).
const MAX_BINDINGS: usize = 32;

struct Registry<E> {
    next_handle: u64,
    apps: HashMap<u64, Arc<Mutex<AppState<E>>>>,
    by_bundle: HashMap<String, u64>,
    order: VecDeque<u64>,
}

impl<E> Default for Registry<E> {
    fn default() -> Self {
        Self {
            next_handle: 1,
            apps: HashMap::new(),
            by_bundle: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

/// One backend's sessions of the API.
pub(crate) struct Session<P: Platform> {
    platform: P,
    context: Arc<HostContext>,
    registry: Mutex<Registry<P::Element>>,
}

impl<P: Platform> Session<P> {
    pub(crate) fn new(platform: P, context: Arc<HostContext>) -> Self {
        Self {
            platform,
            context,
            registry: Mutex::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn platform(&self) -> &P {
        &self.platform
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry<P::Element>> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `list_apps()`.
    pub(crate) fn list_apps(&self) -> Result<Vec<AppEntry>> {
        match self.platform.discovery() {
            Discovery::Workspace(workspace) => Ok(workspace
                .running_apps()?
                .into_iter()
                .map(|app| AppEntry {
                    id: app.bundle_id,
                    name: app.name,
                })
                .collect()),
            Discovery::Windows(windows) => windows.list_apps(),
        }
    }

    /// `permissions_status()`.
    pub(crate) fn permissions(&self) -> Value {
        self.platform.permissions().to_json()
    }

    /// `get_state(emit)`: apps (a transport failure reads as none),
    /// permissions, the allowlist, the platform.
    pub(crate) fn get_state(&self, emit: bool) -> Result<Value> {
        let started = Instant::now();
        let apps = match self.list_apps() {
            Ok(apps) => apps,
            Err(error) if error.code == ErrorCode::TransportError => Vec::new(),
            Err(error) => return Err(error),
        };
        let platform = self.platform.kind().wire_name();
        let state = json!({
            "apps": apps_json(&apps),
            "permissions": self.permissions(),
            "allowlist": self.context.policy.summary(),
            "platform": platform,
        });
        if emit {
            self.context.emit_get_state(platform, started);
        }
        Ok(state)
    }

    /// Register a freshly bound app, returning its handle.
    fn register(&self, state: AppState<P::Element>) -> BoundApp {
        let mut registry = self.registry();
        let handle = registry.next_handle;
        registry.next_handle += 1;
        let bound = BoundApp {
            handle,
            bundle_id: state.bundle_id.clone(),
            name: state.name.clone(),
            pid: state.target,
            state: state.state.clone(),
        };
        registry.by_bundle.insert(state.bundle_id.clone(), handle);
        registry.apps.insert(handle, Arc::new(Mutex::new(state)));
        registry.order.push_back(handle);
        while registry.order.len() > MAX_BINDINGS {
            if let Some(evicted) = registry.order.pop_front() {
                registry.apps.remove(&evicted);
                registry.by_bundle.retain(|_, handle| *handle != evicted);
            }
        }
        bound
    }

    /// The binding `get_app` reuses: the bundle's current one when it still
    /// addresses the same pid (or window).
    fn reusable(&self, bundle_id: &str, target: i64) -> Option<BoundApp> {
        let (handle, app) = {
            let registry = self.registry();
            let handle = *registry.by_bundle.get(bundle_id)?;
            (handle, Arc::clone(registry.apps.get(&handle)?))
        };
        let app = app.lock().unwrap_or_else(PoisonError::into_inner);
        (app.target == target).then(|| BoundApp {
            handle,
            bundle_id: app.bundle_id.clone(),
            name: app.name.clone(),
            pid: app.target,
            state: app.state.clone(),
        })
    }

    /// Run one App method on a bound handle.
    pub(crate) fn call(&self, handle: u64, call: AppCall) -> Result<Value> {
        let app = self.registry().apps.get(&handle).cloned().ok_or_else(|| {
            ComputerUseError::new(
                ErrorCode::AppNotRunning,
                "this App binding is no longer known to Prime Agent; call get_app again to \
                 re-bind it",
            )
            .with_details(json!({"handle": handle}))
        })?;
        let mut app = app.lock().unwrap_or_else(PoisonError::into_inner);
        self.dispatch(&mut app, call)
    }
}

/// The `list_apps()` rows.
pub(crate) fn apps_json(apps: &[AppEntry]) -> Value {
    Value::Array(
        apps.iter()
            .map(|app| json!({"id": app.id, "name": app.name, "running": true}))
            .collect(),
    )
}

/// Where a bound app's per-app instruction guides live (the skill's
/// `references/app-instructions`, sent by the kernel client).
pub(crate) type InstructionsDir = Option<PathBuf>;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;
