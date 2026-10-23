//! Permission modes, Claude Code's names: how much a session asks before it
//! acts, on top of the rules.

use std::fmt;

/// A session's permission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionMode {
    /// The rules decide; an action no rule covers takes the configured
    /// fallback (`allow` unless the settings say `ask`).
    Default,
    /// File edits inside the working directories (and the file commands
    /// `mkdir`, `touch`, `rm`, `rmdir`, `mv`, `cp`, `sed` on them) run
    /// without asking.
    AcceptEdits,
    /// The agent plans: file edits are refused and commands other than
    /// read-only ones ask. The native `/plan` confines the kernel; the
    /// cycle key switches both together.
    Plan,
    /// Everything runs without asking, except what a deny or ask rule
    /// covers.
    BypassPermissions,
    /// Nothing asks: what would ask is refused unless an allow rule covers
    /// it.
    DontAsk,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 5] = [
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::BypassPermissions,
        PermissionMode::DontAsk,
    ];

    /// The settings and command spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
            PermissionMode::DontAsk => "dontAsk",
        }
    }

    /// Parse a mode name (case-insensitive; `bypass` and `accept-edits`
    /// spellings accepted).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let folded: String = text
            .trim()
            .chars()
            .filter(|ch| *ch != '-' && *ch != '_')
            .flat_map(char::to_lowercase)
            .collect();
        match folded.as_str() {
            "default" => Some(PermissionMode::Default),
            "acceptedits" => Some(PermissionMode::AcceptEdits),
            "plan" => Some(PermissionMode::Plan),
            "bypasspermissions" | "bypass" => Some(PermissionMode::BypassPermissions),
            "dontask" => Some(PermissionMode::DontAsk),
            _ => None,
        }
    }

    /// Claude Code's display title.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            PermissionMode::Default => "Default",
            PermissionMode::AcceptEdits => "Accept edits",
            PermissionMode::Plan => "Plan Mode",
            PermissionMode::BypassPermissions => "Bypass Permissions",
            PermissionMode::DontAsk => "Don't Ask",
        }
    }

    /// The indicator row under the editor; `None` in the default mode
    /// (Claude Code shows nothing there either).
    #[must_use]
    pub fn indicator(self, cycles: bool) -> Option<String> {
        let label = match self {
            PermissionMode::Default => return None,
            PermissionMode::AcceptEdits => "\u{25b8}\u{25b8} accept edits on",
            PermissionMode::Plan => "\u{2016} plan mode on",
            PermissionMode::BypassPermissions => "\u{25b8}\u{25b8} bypass permissions on",
            PermissionMode::DontAsk => "\u{25b8}\u{25b8} don't ask on",
        };
        Some(if cycles {
            format!("{label} (shift+tab to cycle)")
        } else {
            label.to_string()
        })
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The modes the cycle key steps through, in Claude Code's order: default,
/// accept edits (only where something asks: with the `allow` fallback it
/// changes nothing), plan, and bypass when the user enabled it.
#[must_use]
pub fn cycle(asks_by_default: bool, bypass_available: bool) -> Vec<PermissionMode> {
    let mut modes = vec![PermissionMode::Default];
    if asks_by_default {
        modes.push(PermissionMode::AcceptEdits);
    }
    modes.push(PermissionMode::Plan);
    if bypass_available {
        modes.push(PermissionMode::BypassPermissions);
    }
    modes
}

/// The mode after `current` in `modes`; a mode outside the cycle (`dontAsk`)
/// steps to the first.
#[must_use]
pub fn next_in(modes: &[PermissionMode], current: PermissionMode) -> PermissionMode {
    modes
        .iter()
        .position(|mode| *mode == current)
        .and_then(|index| modes.get((index + 1) % modes.len()))
        .or_else(|| modes.first())
        .copied()
        .unwrap_or(PermissionMode::Default)
}

/// The session commands that switch from `current` to `next`: the
/// permission mode, and the native plan mode on entering or leaving plan.
#[must_use]
pub fn switch_commands(current: PermissionMode, next: PermissionMode) -> Vec<String> {
    let mut commands = Vec::new();
    if current == PermissionMode::Plan && next != PermissionMode::Plan {
        commands.push("/plan off".to_string());
    }
    commands.push(format!("/permissions mode {next}"));
    if next == PermissionMode::Plan && current != PermissionMode::Plan {
        commands.push("/plan on".to_string());
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_from_claude_code_spellings() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(
            PermissionMode::parse("accept-edits"),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            PermissionMode::parse("bypass"),
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(PermissionMode::parse("auto"), None);
    }

    #[test]
    fn the_cycle_follows_claude_codes_order() {
        assert_eq!(
            cycle(true, true),
            vec![
                PermissionMode::Default,
                PermissionMode::AcceptEdits,
                PermissionMode::Plan,
                PermissionMode::BypassPermissions,
            ]
        );
        // With nothing asking, accept edits would change nothing.
        let quiet = cycle(false, false);
        assert_eq!(quiet, vec![PermissionMode::Default, PermissionMode::Plan]);
        assert_eq!(
            next_in(&quiet, PermissionMode::Plan),
            PermissionMode::Default
        );
        assert_eq!(
            next_in(&quiet, PermissionMode::DontAsk),
            PermissionMode::Default
        );
    }

    #[test]
    fn switching_into_and_out_of_plan_drives_the_native_plan_mode() {
        assert_eq!(
            switch_commands(PermissionMode::AcceptEdits, PermissionMode::Plan),
            vec!["/permissions mode plan", "/plan on"]
        );
        assert_eq!(
            switch_commands(PermissionMode::Plan, PermissionMode::Default),
            vec!["/plan off", "/permissions mode default"]
        );
        assert_eq!(
            switch_commands(PermissionMode::Default, PermissionMode::AcceptEdits),
            vec!["/permissions mode acceptEdits"]
        );
    }
}
