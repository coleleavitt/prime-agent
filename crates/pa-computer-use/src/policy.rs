//! The allowlist gate: the hard gate of the computer-use safety model.
//!
//! The user edits `<agent dir>/settings/computer-use.toml`; every binding and
//! every action re-reads it, and nothing here ever writes it. A missing or
//! unparsable file denies every app. The decision core ([`gate`]) is pure;
//! [`Policy`] is the thin shell that reads the file.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// OS authentication surfaces: always refused, whatever the file says.
pub const SYSTEM_DENY: [&str; 2] = ["com.apple.loginwindow", "com.apple.ScreenSaver"];

/// One app's risk label (`[risk]` in the settings file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Low,
    Medium,
    High,
}

impl Risk {
    fn parse(label: &str) -> Option<Self> {
        match label {
            "low" => Some(Risk::Low),
            "medium" => Some(Risk::Medium),
            "high" => Some(Risk::High),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

/// The normalized settings; the defaults deny every app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub allowed: Vec<String>,
    pub blocked: Vec<String>,
    /// The built-in [`SYSTEM_DENY`] entries first, then the file's own.
    pub system_deny: Vec<String>,
    /// In file order.
    pub risk: Vec<(String, Risk)>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            allowed: Vec::new(),
            blocked: Vec::new(),
            system_deny: SYSTEM_DENY.iter().map(ToString::to_string).collect(),
            risk: Vec::new(),
        }
    }
}

impl Settings {
    /// Normalize a decoded settings document, ignoring every invalid piece.
    #[must_use]
    pub fn from_document(document: &toml::Table) -> Self {
        let apps = document.get("apps").and_then(toml::Value::as_table);
        let list = |key: &str| apps.and_then(|table| table.get(key));
        let mut risk: Vec<(String, Risk)> = Vec::new();
        if let Some(table) = document.get("risk").and_then(toml::Value::as_table) {
            for (bundle_id, label) in table {
                if let Some(label) = label.as_str().and_then(Risk::parse) {
                    risk.push((bundle_id.clone(), label));
                }
            }
        }
        Self {
            allowed: bundle_list(list("allowed"), Vec::new()),
            blocked: bundle_list(list("blocked"), Vec::new()),
            system_deny: bundle_list(
                document.get("system_deny"),
                SYSTEM_DENY.iter().map(ToString::to_string).collect(),
            ),
            risk,
        }
    }

    /// Read the settings file, falling back to the defaults on any read,
    /// decode or parse error.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else {
            return Self::default();
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return Self::default();
        };
        match text.parse::<toml::Table>() {
            Ok(document) => Self::from_document(&document),
            Err(_) => Self::default(),
        }
    }

    fn risk_of(&self, bundle_id: &str) -> Risk {
        self.risk
            .iter()
            .find(|(key, _)| key == bundle_id)
            .map_or(Risk::Medium, |&(_, risk)| risk)
    }
}

/// Keep the string entries of one settings list after `base`, in order,
/// without duplicates.
fn bundle_list(value: Option<&toml::Value>, mut kept: Vec<String>) -> Vec<String> {
    if let Some(entries) = value.and_then(toml::Value::as_array) {
        for entry in entries {
            if let Some(entry) = entry.as_str() {
                if !kept.iter().any(|existing| existing == entry) {
                    kept.push(entry.to_string());
                }
            }
        }
    }
    kept
}

/// Why the gate refused an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    SystemDeny,
    Blocked,
    NotAllowlisted,
}

/// One gate decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    Allowed {
        risk: Risk,
    },
    Denied {
        denial: Denial,
        /// The actionable denial text naming the settings file.
        reason: String,
        risk: Risk,
    },
}

/// Decide one bundle id against `settings`; the deny-lists win over the allowlist.
#[must_use]
pub fn gate(bundle_id: &str, settings: &Settings, settings_path: &Path) -> GateVerdict {
    let risk = settings.risk_of(bundle_id);
    let path = settings_path.display();
    let denial = if SYSTEM_DENY.contains(&bundle_id)
        || settings.system_deny.iter().any(|entry| entry == bundle_id)
    {
        Denial::SystemDeny
    } else if settings.blocked.iter().any(|entry| entry == bundle_id) {
        Denial::Blocked
    } else if !settings.allowed.iter().any(|entry| entry == bundle_id) {
        Denial::NotAllowlisted
    } else {
        return GateVerdict::Allowed { risk };
    };
    let reason = match denial {
        Denial::SystemDeny => format!(
            "{bundle_id} is on the system deny-list; OS authentication surfaces are always \
             refused. To allow an app, add its bundle id to `apps.allowed` in {path}."
        ),
        Denial::Blocked => format!(
            "{bundle_id} is on the blocked list; remove it from `apps.blocked` in {path} to use \
             it. To allow an app, add its bundle id to `apps.allowed` in the same file."
        ),
        Denial::NotAllowlisted => format!(
            "{bundle_id} is not on the allowlist. To allow it, add its bundle id to \
             `apps.allowed` in {path}:\n\n    [apps]\n    allowed = [\"{bundle_id}\"]\n\nThe \
             allowlist is user-edited; Prime Agent never edits it."
        ),
    };
    GateVerdict::Denied {
        denial,
        reason,
        risk,
    }
}

/// The settings file every gate decision reads, at call time.
#[derive(Debug, Clone)]
pub struct Policy {
    settings_path: PathBuf,
}

impl Policy {
    /// The policy of one agent dir: `<agent_dir>/settings/computer-use.toml`.
    #[must_use]
    pub fn for_agent_dir(agent_dir: &Path) -> Self {
        Self {
            settings_path: agent_dir.join("settings").join("computer-use.toml"),
        }
    }

    #[must_use]
    pub fn settings_path(&self) -> &Path {
        &self.settings_path
    }

    /// Gate one app against the file on disk.
    #[must_use]
    pub fn gate(&self, bundle_id: &str) -> GateVerdict {
        gate(
            bundle_id,
            &Settings::load(&self.settings_path),
            &self.settings_path,
        )
    }

    /// The JSON-shaped allowlist view `get_state()` reports.
    #[must_use]
    pub fn summary(&self) -> Value {
        let settings = Settings::load(&self.settings_path);
        let risk: Map<String, Value> = settings
            .risk
            .iter()
            .map(|(bundle_id, risk)| (bundle_id.clone(), Value::from(risk.as_str())))
            .collect();
        json!({
            "allowed": settings.allowed,
            "blocked": settings.blocked,
            "system_deny": settings.system_deny,
            "risk": risk,
        })
    }
}

#[cfg(test)]
mod tests;
