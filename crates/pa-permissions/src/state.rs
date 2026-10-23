//! One session's permission state: its settings (reloaded when a source
//! file changes), its mode, the rules its answers added, and the compiled
//! policy.

use pa_core::features::FeatureStatus;
use serde_json::json;

use crate::decide::{Action, Decision, Policy};
use crate::mode::{self, PermissionMode};
use crate::rule::{RuleBehavior, RuleValue};
use crate::settings::{
    Fallback,
    PermissionSettings,
    SourceKind,
    SourcePaths,
    SourcedRule,
    Stamp,
    load,
    persist_rule,
    stamp,
};

/// The feature status name (the TUI reads the cycle from it).
pub const STATUS_FEATURE: &str = "permissions";

/// Where an added rule went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// Written to the local settings file.
    LocalFile,
    /// Kept for this session only (an untrusted workspace, or the file
    /// could not be written: the reason).
    Session(String),
}

/// One session's state.
#[derive(Debug)]
pub struct SessionState {
    pub paths: SourcePaths,
    trusted: bool,
    pub settings: PermissionSettings,
    stamp: Stamp,
    mode: PermissionMode,
    /// The mode was chosen in this session (a reload keeps it).
    mode_chosen: bool,
    pub session_rules: Vec<SourcedRule>,
    policy: Policy,
}

impl SessionState {
    /// Load the state of a session reading `paths`, in a workspace
    /// `trusted` or not.
    #[must_use]
    pub fn new(paths: SourcePaths, trusted: bool) -> Self {
        let settings = load(&paths, trusted);
        let stamp = stamp(&paths);
        let mode = settings.default_mode;
        let policy = Policy::new(&settings, &[], mode, &paths.anchors());
        SessionState {
            paths,
            trusted,
            settings,
            stamp,
            mode,
            mode_chosen: false,
            session_rules: Vec::new(),
            policy,
        }
    }

    /// Re-read the settings when a source file changed; whether the
    /// published status changed with them.
    pub fn refresh(&mut self) -> bool {
        let current = stamp(&self.paths);
        if current == self.stamp {
            return false;
        }
        let before = self.status();
        self.reload_with(current);
        self.status() != before
    }

    /// Re-read the settings now.
    pub fn reload(&mut self) {
        let current = stamp(&self.paths);
        self.reload_with(current);
    }

    fn reload_with(&mut self, current: Stamp) {
        self.settings = load(&self.paths, self.trusted);
        self.stamp = current;
        if !self.mode_chosen {
            self.mode = self.settings.default_mode;
        }
        self.rebuild();
    }

    fn rebuild(&mut self) {
        self.policy = Policy::new(
            &self.settings,
            &self.session_rules,
            self.mode,
            &self.paths.anchors(),
        );
    }

    #[must_use]
    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Switch the mode.
    ///
    /// # Errors
    ///
    /// `bypassPermissions` is disabled by a settings source.
    pub fn set_mode(&mut self, mode: PermissionMode) -> Result<(), String> {
        if mode == PermissionMode::BypassPermissions && self.settings.bypass_disabled {
            return Err(
                "bypassPermissions is disabled by settings (disableBypassPermissionsMode)"
                    .to_string(),
            );
        }
        self.mode = mode;
        self.mode_chosen = true;
        self.rebuild();
        Ok(())
    }

    /// Decide `action` against the current settings (see [`Self::refresh`]).
    pub fn decide(&mut self, action: &Action) -> Decision {
        self.refresh();
        self.policy.decide(action)
    }

    /// Add `rule` as a `behavior` rule: to the local settings file in a
    /// trusted workspace, else for this session.
    pub fn add_rule(&mut self, behavior: RuleBehavior, rule: &str) -> Saved {
        let saved = if self.trusted {
            match persist_rule(&self.paths.local_file(), behavior, rule) {
                Ok(_) => Saved::LocalFile,
                Err(error) => Saved::Session(error),
            }
        } else {
            Saved::Session("the workspace is not trusted".to_string())
        };
        match &saved {
            Saved::LocalFile => self.reload(),
            Saved::Session(_) => {
                let value = RuleValue::parse(rule);
                let known = self
                    .session_rules
                    .iter()
                    .any(|known| known.behavior == behavior && known.value == value);
                if !known {
                    self.session_rules.push(SourcedRule {
                        behavior,
                        value,
                        source: SourceKind::Session,
                        root: self.paths.cwd.clone(),
                    });
                }
                self.rebuild();
            }
        }
        saved
    }

    /// Remove `rule` from the local settings file and this session; whether
    /// it was found in either.
    ///
    /// # Errors
    ///
    /// The local settings file cannot be rewritten.
    pub fn remove_rule(&mut self, rule: &str) -> Result<bool, String> {
        let value = RuleValue::parse(rule);
        let before = self.session_rules.len();
        self.session_rules.retain(|known| known.value != value);
        let mut removed = self.session_rules.len() != before;
        removed |= crate::settings::remove_rule(&self.paths.local_file(), rule)?;
        self.reload();
        Ok(removed)
    }

    /// The modes the cycle key steps through; `None` when the key should
    /// stay the plain plan toggle (nothing asks by default, bypass is off,
    /// and the mode is the default one).
    #[must_use]
    pub fn cycle(&self) -> Option<Vec<PermissionMode>> {
        let asks = self.settings.fallback == Fallback::Ask;
        let bypass = self.settings.bypass_available && !self.settings.bypass_disabled;
        if !asks && !bypass && self.mode == PermissionMode::Default {
            return None;
        }
        Some(mode::cycle(asks, bypass))
    }

    /// The status the session publishes: the mode, its indicator row, and
    /// the commands the cycle key sends next.
    #[must_use]
    pub fn status(&self) -> FeatureStatus {
        let cycle = self.cycle();
        let indicator = self.mode.indicator(cycle.is_some());
        let mut status = json!({
            "mode": self.mode.as_str(),
            "panel": indicator.into_iter().collect::<Vec<_>>(),
        });
        if let Some(modes) = &cycle {
            let next = mode::next_in(modes, self.mode);
            status["cycle"] = json!(mode::switch_commands(self.mode, next));
        }
        FeatureStatus {
            feature: STATUS_FEATURE.to_string(),
            line: None,
            status,
        }
    }
}
