//! The kernel host requests: backend detection, routing and the wire format.
//!
//! Five request types serve the kernel's `computer_use` package:
//!
//! | type | payload | result |
//! |---|---|---|
//! | `computer_use.get_state` | `{emit}` | the `get_state()` dict |
//! | `computer_use.list_apps` | `{}` | `[{id, name, running}]` |
//! | `computer_use.permissions_status` | `{}` | the permissions dict |
//! | `computer_use.get_app` | `{spec, instructions_dir}` | `{handle, bundle_id, name, pid, state}` |
//! | `computer_use.app` | `{handle, method, ...args}` | the method's result |
//!
//! Every reply is `{"ok": result}` or `{"error": {code, message, details}}`;
//! the client raises the latter as `ComputerUseError`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

use crate::error::{invalid, transport, ComputerUseError, Result};
use crate::permissions::{PermissionReport, PermissionState};
use crate::platform::{MouseButton, PasteFormat, Platform, PlatformKind, ScrollDirection};
use crate::policy::Policy;
use crate::session::{
    apps_json, ActionArg, AppCall, HostContext, IndexArg, PointArg, Session, TargetArg, TextArg,
    Timing,
};
use crate::spec::AppSpec;
use crate::telemetry::TelemetrySink;

/// The kernel host request types this crate serves.
pub const REQUEST_TYPES: [&str; 5] = [
    "computer_use.get_state",
    "computer_use.list_apps",
    "computer_use.permissions_status",
    "computer_use.get_app",
    "computer_use.app",
];

/// How a host embeds computer use.
pub struct HostConfig {
    /// The agent dir: the allowlist is `<agent_dir>/settings/computer-use.toml`
    /// and screenshots land in `<agent_dir>/tmp/computer-use/`.
    pub agent_dir: PathBuf,
    pub telemetry: Arc<dyn TelemetrySink>,
}

/// One backend's session, type-erased for routing.
pub(crate) trait Backend: Send + Sync {
    fn get_state(&self, emit: bool) -> Result<Value>;
    fn list_apps(&self) -> Result<Value>;
    fn permissions(&self) -> Value;
    fn get_app(&self, spec: &AppSpec, instructions_dir: Option<PathBuf>) -> Result<Value>;
    fn call(&self, handle: u64, call: AppCall) -> Result<Value>;
}

impl<P: Platform> Backend for Session<P> {
    fn get_state(&self, emit: bool) -> Result<Value> {
        Session::get_state(self, emit)
    }

    fn list_apps(&self) -> Result<Value> {
        Session::list_apps(self).map(|apps| apps_json(&apps))
    }

    fn permissions(&self) -> Value {
        Session::permissions(self)
    }

    fn get_app(&self, spec: &AppSpec, instructions_dir: Option<PathBuf>) -> Result<Value> {
        Session::get_app(self, spec, instructions_dir).map(|bound| bound.to_json())
    }

    fn call(&self, handle: u64, call: AppCall) -> Result<Value> {
        Session::call(self, handle, call)
    }
}

/// The computer-use host of one agent session.
pub struct ComputerUse {
    context: Arc<HostContext>,
    backends: Backends,
}

impl ComputerUse {
    #[must_use]
    pub fn new(config: HostConfig) -> Self {
        let context = Arc::new(HostContext::new(
            Policy::for_agent_dir(&config.agent_dir),
            config.telemetry,
            Timing::default(),
        ));
        Self {
            backends: Backends::new(config.agent_dir),
            context,
        }
    }

    /// Serve one request on a blocking thread (backend calls block on AX,
    /// AT-SPI and subprocesses), so the async runtime keeps running.
    pub async fn handle(self: &Arc<Self>, request_type: &str, payload: Value) -> Value {
        let host = Arc::clone(self);
        let request_type = request_type.to_string();
        run_blocking(move || host.serve(&request_type, &payload)).await
    }

    /// Serve one request on the calling thread.
    #[must_use]
    pub fn handle_blocking(&self, request_type: &str, payload: &Value) -> Value {
        reply(self.serve(request_type, payload))
    }

    fn serve(&self, request_type: &str, payload: &Value) -> Result<Value> {
        let backend = self.backends.detect(&self.context);
        route(backend, &self.context, request_type, payload)
    }
}

/// Run one request's work on the blocking pool and wrap its reply.
async fn run_blocking(work: impl FnOnce() -> Result<Value> + Send + 'static) -> Value {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => reply(result),
        Err(join) => reply(Err(transport(format!(
            "the computer-use request failed: {join}"
        )))),
    }
}

fn reply(result: Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"ok": result}),
        Err(error) => json!({"error": error.to_wire()}),
    }
}

fn malformed(request_type: &str) -> ComputerUseError {
    invalid(format!("malformed {request_type} request"))
}

/// Route one request to the detected backend (or the no-backend answers).
pub(crate) fn route(
    backend: Option<&dyn Backend>,
    context: &HostContext,
    request_type: &str,
    payload: &Value,
) -> Result<Value> {
    match request_type {
        "computer_use.get_state" => {
            let emit = payload.get("emit").and_then(Value::as_bool).unwrap_or(true);
            match backend {
                Some(backend) => backend.get_state(emit),
                None => Ok(no_backend_state(context, emit)),
            }
        }
        "computer_use.list_apps" => match backend {
            Some(backend) => backend.list_apps(),
            None => Err(transport(
                "computer use backend unavailable: listing apps needs the macOS workspace",
            )),
        },
        "computer_use.permissions_status" => Ok(match backend {
            Some(backend) => backend.permissions(),
            None => unprobed_grants().to_json(),
        }),
        "computer_use.get_app" => {
            let backend = backend.ok_or_else(no_backend)?;
            let spec = payload
                .get("spec")
                .and_then(AppSpec::from_wire)
                .ok_or_else(|| malformed(request_type))?;
            let instructions_dir = payload
                .get("instructions_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            backend.get_app(&spec, instructions_dir)
        }
        "computer_use.app" => {
            let backend = backend.ok_or_else(no_backend)?;
            let handle = payload
                .get("handle")
                .and_then(Value::as_u64)
                .ok_or_else(|| malformed(request_type))?;
            let call = decode_call(payload).ok_or_else(|| malformed(request_type))?;
            backend.call(handle, call)
        }
        other => Err(invalid(format!(
            "unknown computer-use request type {other:?}"
        ))),
    }
}

fn no_backend() -> ComputerUseError {
    transport(
        "computer use backend unavailable: no macOS frameworks, no niri Wayland session, and no \
         Linux X11 tools on this host",
    )
}

/// Off darwin the TCC probes never run: both grants read unknown.
fn unprobed_grants() -> PermissionReport {
    PermissionReport::Grants {
        accessibility: PermissionState::Unknown,
        screen_recording: PermissionState::Unknown,
    }
}

fn no_backend_state(context: &HostContext, emit: bool) -> Value {
    let started = std::time::Instant::now();
    let state = json!({
        "apps": [],
        "permissions": unprobed_grants().to_json(),
        "allowlist": context.policy.summary(),
        "platform": null,
    });
    if emit {
        context.emit_get_state("unknown", started);
    }
    state
}

fn decode_point(value: &Value) -> Option<PointArg> {
    let repr = value.get("repr")?.as_str()?.to_string();
    match (
        value.get("x").and_then(Value::as_f64),
        value.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => Some(PointArg::Valid { x, y, repr }),
        _ => Some(PointArg::Invalid { repr }),
    }
}

fn decode_target(value: &Value) -> Option<TargetArg> {
    match value.get("kind")?.as_str()? {
        "index" => Some(TargetArg::Index(value.get("index")?.as_i64()?)),
        "point" => Some(TargetArg::Point(decode_point(value.get("point")?)?)),
        "invalid" => Some(TargetArg::Invalid(value.get("type")?.as_str()?.to_string())),
        _ => None,
    }
}

fn decode_text(value: &Value) -> Option<TextArg> {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return Some(TextArg::Text(text.to_string()));
    }
    Some(TextArg::NotText(value.get("type")?.as_str()?.to_string()))
}

fn decode_index(value: &Value) -> Option<IndexArg> {
    if let Some(index) = value.get("index").and_then(Value::as_i64) {
        return Some(IndexArg::Valid(index));
    }
    Some(IndexArg::Invalid(value.get("type")?.as_str()?.to_string()))
}

fn string(payload: &Value, key: &str) -> Option<String> {
    payload.get(key)?.as_str().map(ToString::to_string)
}

/// An optional string argument; `Err` for a present non-string.
fn optional_string(payload: &Value, key: &str) -> Result<Option<String>, ()> {
    match payload.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

fn count(payload: &Value, key: &str) -> Option<u32> {
    u32::try_from(payload.get(key)?.as_u64()?).ok()
}

/// Decode one `computer_use.app` call; `None` for a malformed one (the
/// kernel client validates every Python-typed argument before sending).
pub(crate) fn decode_call(payload: &Value) -> Option<AppCall> {
    let method = payload.get("method")?.as_str()?;
    Some(match method {
        "get_ax_state" => AppCall::GetAxState {
            diff: payload.get("diff")?.as_bool()?,
        },
        "get_screenshot" => AppCall::GetScreenshot,
        "get_text_regions" => AppCall::GetTextRegions,
        "click" => AppCall::Click {
            target: decode_target(payload.get("target")?)?,
            button: match payload.get("button")?.as_str()? {
                "left" => MouseButton::Left,
                "right" => MouseButton::Right,
                "middle" => MouseButton::Middle,
                _ => return None,
            },
            count: count(payload, "count").filter(|count| (1..=10).contains(count))?,
        },
        "drag" => AppCall::Drag {
            from: decode_point(payload.get("from")?)?,
            to: decode_point(payload.get("to")?)?,
        },
        "scroll" => AppCall::Scroll {
            target: decode_target(payload.get("target")?)?,
            direction: match payload.get("direction")?.as_str()? {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "left" => ScrollDirection::Left,
                "right" => ScrollDirection::Right,
                _ => return None,
            },
            pages: count(payload, "pages").filter(|pages| *pages >= 1)?,
        },
        "press_key" => AppCall::PressKey {
            key: decode_text(payload.get("key")?)?,
        },
        "type_text" => AppCall::TypeText {
            text: decode_text(payload.get("text")?)?,
        },
        "set_value" => AppCall::SetValue {
            index: decode_index(payload.get("element_index")?)?,
            value: string(payload, "value")?,
        },
        "select_text" => AppCall::SelectText {
            index: decode_index(payload.get("element_index")?)?,
            text: string(payload, "text").filter(|text| !text.is_empty())?,
            prefix: optional_string(payload, "prefix").ok()?,
            suffix: optional_string(payload, "suffix").ok()?,
        },
        "perform_secondary_action" => AppCall::SecondaryAction {
            index: decode_index(payload.get("element_index")?)?,
            action: {
                let action = payload.get("action")?;
                match action.get("name").and_then(Value::as_str) {
                    Some(name) => ActionArg::Name(name.to_string()),
                    None => ActionArg::NotText(action.get("str")?.as_str()?.to_string()),
                }
            },
        },
        "paste" => AppCall::Paste {
            text: string(payload, "text")?,
            format: match payload.get("format")?.as_str()? {
                "text" => PasteFormat::Text,
                "md" => PasteFormat::Markdown,
                "html" => PasteFormat::Html,
                _ => return None,
            },
        },
        "activate" => AppCall::Activate,
        "is_frontmost" => AppCall::IsFrontmost,
        _ => return None,
    })
}

/// The real backends, built on first use (one slot per [`PlatformKind`]).
struct Backends {
    agent_dir: PathBuf,
    built: [OnceLock<Option<Box<dyn Backend>>>; 3],
}

impl Backends {
    fn new(agent_dir: PathBuf) -> Self {
        Self {
            agent_dir,
            built: [OnceLock::new(), OnceLock::new(), OnceLock::new()],
        }
    }

    /// The backend this host runs now: macOS on darwin; on Linux the niri
    /// Wayland backend under a niri session (ahead of X11: niri usually
    /// exports DISPLAY through xwayland-satellite too), else X11 when
    /// xdotool is on PATH.
    fn detect(&self, context: &Arc<HostContext>) -> Option<&dyn Backend> {
        let kind = detect_kind(&crate::process::SystemTools)?;
        let slot = match kind {
            PlatformKind::Mac => &self.built[0],
            PlatformKind::X11 => &self.built[1],
            PlatformKind::Wayland => &self.built[2],
        };
        slot.get_or_init(|| self.build(kind, context)).as_deref()
    }

    /// The real platform of `kind` on this target (`None` for a kind the
    /// target never detects).
    fn build(&self, kind: PlatformKind, context: &Arc<HostContext>) -> Option<Box<dyn Backend>> {
        let context = Arc::clone(context);
        let agent_dir = &self.agent_dir;
        match kind {
            // The macOS and Wayland backends land in the following commits.
            PlatformKind::Mac | PlatformKind::Wayland => {
                let _ = (agent_dir, context);
                None
            }
            PlatformKind::X11 => {
                #[cfg(target_os = "linux")]
                return Some(Box::new(Session::new(
                    crate::platform::x11::native::platform(agent_dir),
                    context,
                )));
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = (agent_dir, context);
                    None
                }
            }
        }
    }
}

/// The detection rule, over the environment seam.
pub(crate) fn detect_kind(tools: &dyn crate::process::Tools) -> Option<PlatformKind> {
    if cfg!(target_os = "macos") {
        return Some(PlatformKind::Mac);
    }
    if !cfg!(target_os = "linux") {
        return None;
    }
    if niri_session(tools) {
        return Some(PlatformKind::Wayland);
    }
    tools.which("xdotool").map(|_| PlatformKind::X11)
}

/// `WAYLAND_DISPLAY` plus a `NIRI_SOCKET` naming a live socket.
fn niri_session(tools: &dyn crate::process::Tools) -> bool {
    if tools
        .env("WAYLAND_DISPLAY")
        .is_none_or(|display| display.is_empty())
    {
        return false;
    }
    tools
        .env("NIRI_SOCKET")
        .filter(|path| !path.is_empty())
        .is_some_and(|path| tools.is_socket(&path))
}

#[cfg(test)]
mod tests;
