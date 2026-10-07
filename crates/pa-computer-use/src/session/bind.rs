//! `get_app`: resolve a spec, gate it, and bind one app.
//!
//! macOS resolves running apps (and launches a not-yet-running one, gated
//! before anything starts); X11 and Wayland attach to a running window.

use serde_json::{json, Value};

use super::{AppState, BoundApp, InstructionsDir, Session};
use crate::error::{head, invalid, ComputerUseError, ErrorCode, Result};
use crate::permissions::PermissionState;
use crate::platform::{Discovery, Platform, PlatformKind, RunningApp, WindowDirectory, Workspace};
use crate::policy::GateVerdict;
use crate::pyfmt::{casefold, casefold_eq, repr_str};
use crate::spec::{is_blank, AppSpec, SpecKey, SpecShape};

/// The Accessibility refusal of a bind (before any launch).
fn accessibility_missing(reported: PermissionState) -> ComputerUseError {
    ComputerUseError::new(
        ErrorCode::PermissionsNotGranted,
        "the Accessibility grant is missing or unknown; allow Prime Agent in System Settings > \
         Privacy & Security > Accessibility, then retry",
    )
    .with_details(json!({"permission": "accessibility", "reported": reported.as_str()}))
}

/// Validate a dict spec into its one (kind, value) pair.
fn spec_value(spec: &AppSpec) -> Result<(SpecKey, &str)> {
    let keys = match &spec.shape {
        SpecShape::Dict { keys, .. } => keys,
        SpecShape::Text(_) => return Err(not_a_spec("str")),
        SpecShape::Other(type_name) => return Err(not_a_spec(type_name)),
    };
    let kinds: Vec<SpecKey> = SpecKey::ALL
        .into_iter()
        .filter(|&key| spec.has_key(key))
        .collect();
    let [kind] = kinds[..] else {
        return Err(
            invalid("a dict app spec must carry exactly one of bundle_id, name, or path")
                .with_details(json!({"keys": Value::Array(keys.clone())})),
        );
    };
    match spec.entry(kind) {
        Some(value) if !is_blank(value) => Ok((kind, value)),
        _ => Err(
            invalid(format!("{} must be a non-empty string", kind.as_str()))
                .with_details(json!({"kind": kind.as_str()})),
        ),
    }
}

fn not_a_spec(type_name: &str) -> ComputerUseError {
    invalid(format!(
        "the app spec must be a string or a dict, got {type_name}"
    ))
    .with_details(json!({"spec": type_name}))
}

/// Compare two filesystem paths canonically (`Path.resolve()`).
fn same_path(running: Option<&str>, wanted: &str) -> bool {
    let Some(running) = running.filter(|path| !path.is_empty()) else {
        return false;
    };
    let canonical =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(std::path::Path::new(running)) == canonical(&expand_user(wanted))
}

/// `Path(path).expanduser()`.
pub(crate) fn expand_user(path: &str) -> std::path::PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let home = std::env::home_dir().unwrap_or_default();
            home.join(rest.trim_start_matches('/'))
        }
        _ => std::path::PathBuf::from(path),
    }
}

/// Match a spec against the running apps: a string matches a bundle id
/// exactly or a name case-insensitively; a dict matches its one key.
fn resolve_running(apps: Vec<RunningApp>, spec: &AppSpec) -> Result<Vec<RunningApp>> {
    if let SpecShape::Text(text) = &spec.shape {
        if is_blank(text) {
            return Err(invalid("the app spec must not be empty").with_details(json!({"spec": ""})));
        }
        let wanted = casefold(text);
        return Ok(apps
            .into_iter()
            .filter(|app| app.bundle_id == *text || casefold(&app.name) == wanted)
            .collect());
    }
    let (kind, value) = spec_value(spec)?;
    Ok(apps
        .into_iter()
        .filter(|app| match kind {
            SpecKey::BundleId => app.bundle_id == value,
            SpecKey::Name => casefold_eq(&app.name, value),
            SpecKey::Path => same_path(app.path.as_deref(), value),
        })
        .collect())
}

impl<P: Platform> Session<P> {
    /// `get_app(spec)`.
    pub(crate) fn get_app(
        &self,
        spec: &AppSpec,
        instructions_dir: InstructionsDir,
    ) -> Result<BoundApp> {
        self.refuse_locked()?;
        match self.platform.discovery() {
            Discovery::Workspace(workspace) => self.bind_running(workspace, spec, instructions_dir),
            Discovery::Windows(windows) => self.bind_window(windows, spec, instructions_dir),
        }
    }

    /// macOS: bind the one allowed running app the spec names, or launch it.
    fn bind_running(
        &self,
        workspace: &dyn Workspace,
        spec: &AppSpec,
        instructions_dir: InstructionsDir,
    ) -> Result<BoundApp> {
        let candidates = resolve_running(workspace.running_apps()?, spec)?;
        let mut allowed = Vec::new();
        let mut first_denial = None;
        for candidate in candidates {
            match self.gate(&candidate.bundle_id) {
                Ok(()) => allowed.push(candidate),
                Err(denial) => {
                    first_denial.get_or_insert(denial);
                }
            }
        }
        if allowed.len() > 1 {
            let mut bundle_ids: Vec<String> =
                allowed.iter().map(|app| app.bundle_id.clone()).collect();
            bundle_ids.sort();
            return Err(ComputerUseError::new(
                ErrorCode::AmbiguousApp,
                format!(
                    "the app spec matched several allowed apps ({}); call get_app with the \
                     bundle_id of the one you want",
                    bundle_ids.join(", ")
                ),
            )
            .with_details(json!({"bundle_ids": bundle_ids})));
        }
        let bound = match (allowed.pop(), first_denial) {
            (Some(bound), _) => bound,
            (None, Some(denial)) => return Err(denial),
            (None, None) => self.launch_and_gate(workspace, spec)?,
        };
        let accessibility = self.platform.permissions().accessibility();
        if accessibility != PermissionState::Ok {
            return Err(accessibility_missing(accessibility));
        }
        if let Some(existing) = self.reusable(&bound.bundle_id, bound.pid) {
            return Ok(existing);
        }
        let mut app = AppState::new(bound.bundle_id, bound.name, bound.pid, instructions_dir);
        // The bind window spans the launch and the settle: revalidate
        // before the first read.
        self.guard(&app)?;
        self.refresh(&mut app, false)?;
        Ok(self.register(app))
    }

    /// Launch the app the spec names, gating its bundle id before anything
    /// starts. An unresolvable spec fails closed: nothing whose bundle id
    /// was not determined beforehand is ever opened.
    fn launch_and_gate(&self, workspace: &dyn Workspace, spec: &AppSpec) -> Result<RunningApp> {
        let accessibility = self.platform.permissions().accessibility();
        if accessibility != PermissionState::Ok {
            return Err(accessibility_missing(accessibility));
        }
        let Some(bundle_id) = prelaunch_bundle_id(workspace, spec)? else {
            return Err(ComputerUseError::new(
                ErrorCode::AppNotAllowed,
                format!(
                    "could not resolve {} to a bundle id without launching it, so Prime Agent \
                     fails closed; ask the user to add the app's bundle id to `apps.allowed` in \
                     {} and call get_app with {{'bundle_id': ...}}",
                    spec.repr,
                    self.context.policy.settings_path().display()
                ),
            )
            .with_details(json!({"spec": head(&spec.display, 64)})));
        };
        if let GateVerdict::Denied { reason, .. } = self.context.policy.gate(&bundle_id) {
            return Err(ComputerUseError::new(ErrorCode::AppNotAllowed, reason)
                .with_details(json!({"bundle_id": bundle_id})));
        }
        let launched = workspace.launch(&bundle_id)?;
        if launched.bundle_id != bundle_id {
            return Err(ComputerUseError::new(
                ErrorCode::AppNotAllowed,
                format!(
                    "launching opened {} instead of the gated {bundle_id}; call get_app with the \
                     bundle_id of the app you want",
                    launched.bundle_id
                ),
            )
            .with_details(json!({
                "gated_bundle_id": bundle_id,
                "launched_bundle_id": launched.bundle_id,
            })));
        }
        Ok(launched)
    }

    /// X11 and Wayland: bind the first window the spec resolves to.
    fn bind_window(
        &self,
        windows: &dyn WindowDirectory,
        spec: &AppSpec,
        instructions_dir: InstructionsDir,
    ) -> Result<BoundApp> {
        let Some(window) = windows.resolve(spec)?.into_iter().next() else {
            let place = match self.platform.kind() {
                PlatformKind::Wayland => "in the niri session",
                PlatformKind::X11 | PlatformKind::Mac => "on the linux desktop",
            };
            let shown = head(&spec.display, 64);
            return Err(ComputerUseError::new(
                ErrorCode::AppNotRunning,
                format!(
                    "{} has no window {place}; start the app yourself and call get_app again",
                    repr_str(shown)
                ),
            )
            .with_details(json!({"spec": shown})));
        };
        self.gate(&window.app_id)?;
        if let Some(existing) = self.reusable(&window.app_id, window.window_id) {
            return Ok(existing);
        }
        let mut app = AppState::new(
            window.app_id.clone(),
            window.app_id,
            window.window_id,
            instructions_dir,
        );
        self.refresh(&mut app, false)?;
        Ok(self.register(app))
    }
}

/// The bundle id a spec names without launching anything, or `None`.
///
/// A dotted string is a bundle id only when it names a running app or no
/// installed app answers to it as a display name (a display name can carry
/// a dot too: "Acme 1.0"); other names resolve through Spotlight, paths
/// read their bundle's id.
fn prelaunch_bundle_id(workspace: &dyn Workspace, spec: &AppSpec) -> Result<Option<String>> {
    match &spec.shape {
        SpecShape::Text(text) => {
            if is_blank(text) {
                return Ok(None);
            }
            if text.contains('.') {
                if workspace
                    .running_apps()?
                    .iter()
                    .any(|app| app.bundle_id == *text)
                {
                    return Ok(Some(text.clone()));
                }
                return Ok(Some(
                    workspace
                        .bundle_for_name(text)?
                        .unwrap_or_else(|| text.clone()),
                ));
            }
            workspace.bundle_for_name(text)
        }
        SpecShape::Dict { .. } => {
            let Some(kind) = SpecKey::ALL.into_iter().find(|&key| spec.has_key(key)) else {
                return Ok(None);
            };
            match spec.entry(kind).filter(|value| !is_blank(value)) {
                None => Ok(None),
                Some(value) => match kind {
                    SpecKey::BundleId => Ok(Some(value.to_string())),
                    SpecKey::Path => Ok(workspace.bundle_id_for_path(value)),
                    SpecKey::Name => workspace.bundle_for_name(value),
                },
            }
        }
        SpecShape::Other(_) => Ok(None),
    }
}
