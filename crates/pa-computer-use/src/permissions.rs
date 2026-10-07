//! What each backend needs from the OS, in the `permissions_status()` shape.
//!
//! macOS reports its two TCC grants (probed without ever prompting); X11 has
//! no grants and names the tools it needs; Wayland reports AT-SPI, grim and
//! the virtual-input protocols with a fix-it line for anything missing.

use serde_json::{json, Value};

/// One grant or capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Ok,
    Missing,
    Unknown,
}

impl PermissionState {
    /// Map one probe result: `None` is unknown.
    #[must_use]
    pub fn from_probe(result: Option<bool>) -> Self {
        match result {
            None => PermissionState::Unknown,
            Some(true) => PermissionState::Ok,
            Some(false) => PermissionState::Missing,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionState::Ok => "ok",
            PermissionState::Missing => "missing",
            PermissionState::Unknown => "unknown",
        }
    }
}

/// The macOS fix-it lines.
pub const MAC_HELP_LINES: [&str; 4] = [
    "System Settings > Privacy & Security > Accessibility: add Prime Agent (app control and input).",
    "System Settings > Privacy & Security > Screen Recording: add Prime Agent (window capture).",
    "Prime Agent needs both grants.",
    "Call get_state() to re-check; restart Prime Agent if a fresh Screen Recording grant does not take effect.",
];

/// The X11 backend's note: no TCC analog, both grants read unknown.
pub const X11_HELP_LINE: &str = "Linux: no macOS TCC grants apply; the X11 backend needs DISPLAY \
                                 set and the xdotool, xwininfo, and maim (or scrot) tools on PATH";

/// The Wayland backend's standing note on what apps and windows need.
pub const WAYLAND_APPS_HELP_LINE: &str = "Wayland (niri): apps must expose AT-SPI (GTK/Qt do; \
     Firefox needs accessibility enabled, Chromium/Electron need --force-renderer-accessibility); \
     coordinate input and screenshots need a floating window, because niri does not expose tiled \
     windows' screen positions";

/// One backend's permission report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionReport {
    /// The macOS TCC grants.
    Grants {
        accessibility: PermissionState,
        screen_recording: PermissionState,
    },
    /// The X11 backend: no grants apply.
    X11,
    /// The Wayland backend's real capabilities.
    Wayland {
        accessibility: PermissionState,
        screen_recording: PermissionState,
        pointer: PermissionState,
        keyboard: PermissionState,
        help: Vec<String>,
    },
}

impl PermissionReport {
    #[must_use]
    pub fn accessibility(&self) -> PermissionState {
        match self {
            PermissionReport::Grants { accessibility, .. }
            | PermissionReport::Wayland { accessibility, .. } => *accessibility,
            PermissionReport::X11 => PermissionState::Unknown,
        }
    }

    #[must_use]
    pub fn screen_recording(&self) -> PermissionState {
        match self {
            PermissionReport::Grants {
                screen_recording, ..
            }
            | PermissionReport::Wayland {
                screen_recording, ..
            } => *screen_recording,
            PermissionReport::X11 => PermissionState::Unknown,
        }
    }

    /// The `permissions_status()` dict.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            PermissionReport::Grants {
                accessibility,
                screen_recording,
            } => json!({
                "accessibility": accessibility.as_str(),
                "screen_recording": screen_recording.as_str(),
                "help": MAC_HELP_LINES,
            }),
            PermissionReport::X11 => json!({
                "accessibility": "unknown",
                "screen_recording": "unknown",
                "help": [X11_HELP_LINE],
            }),
            PermissionReport::Wayland {
                accessibility,
                screen_recording,
                pointer,
                keyboard,
                help,
            } => json!({
                "accessibility": accessibility.as_str(),
                "screen_recording": screen_recording.as_str(),
                "input": {"pointer": pointer.as_str(), "keyboard": keyboard.as_str()},
                "help": help,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_results_map_to_states() {
        assert_eq!(PermissionState::from_probe(Some(true)), PermissionState::Ok);
        assert_eq!(
            PermissionState::from_probe(Some(false)),
            PermissionState::Missing
        );
        assert_eq!(PermissionState::from_probe(None), PermissionState::Unknown);
    }

    #[test]
    fn the_mac_report_carries_the_grants_and_the_help_lines() {
        let report = PermissionReport::Grants {
            accessibility: PermissionState::Ok,
            screen_recording: PermissionState::Unknown,
        };
        assert_eq!(
            report.to_json(),
            json!({
                "accessibility": "ok",
                "screen_recording": "unknown",
                "help": MAC_HELP_LINES,
            })
        );
    }

    #[test]
    fn the_help_names_the_settings_paths_and_the_brand() {
        let joined = MAC_HELP_LINES.join("\n");
        assert!(joined.contains("System Settings > Privacy & Security > Accessibility"));
        assert!(joined.contains("System Settings > Privacy & Security > Screen Recording"));
        assert!(joined.contains("Prime Agent"));
    }

    #[test]
    fn x11_reports_unknown_grants_with_the_tool_note() {
        assert_eq!(
            PermissionReport::X11.to_json(),
            json!({"accessibility": "unknown", "screen_recording": "unknown", "help": [X11_HELP_LINE]})
        );
    }
}
