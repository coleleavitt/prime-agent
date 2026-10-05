//! The terminal window title (TS `ProcessTerminal.setTitle`: `OSC 0 ; title BEL`).
//!
//! The session surface titles the window `prime-agent - <session name> - <cwd basename>`
//! (`prime-agent - <cwd basename>` while the session is unnamed) at every attach and whenever
//! the display name changes; the agents view titles it `prime-agent - Agents` at its mount —
//! the TS v0.9.8 formats (`updateTerminalTitle`, `AgentsViewMode`).
//!
//! Divergence from TS (hardening): the first title of the process pushes the shell's title onto
//! the xterm title stack (`CSI 22;0t`) and the exit restore pops it (`CSI 23;0t`), so the shell
//! gets its own title back; TS left its last title behind. Control characters are dropped from
//! the title so a session name cannot end the OSC string early.
//!
//! The sequences are plain bytes in the output stream: they are written unflushed and ride the
//! next frame's flush, never a paint-path wait of their own.

use std::io::{IsTerminal, Stdout, Write};
use std::path::Path;
use std::sync::Mutex;

/// TS `APP_TITLE` (`piConfig.name` of the published package).
const APP_TITLE: &str = "prime-agent";
/// Save the window and icon titles on the xterm title stack.
const PUSH_TITLE: &str = "\x1b[22;0t";
/// Restore the window and icon titles from the xterm title stack.
const POP_TITLE: &str = "\x1b[23;0t";

/// The session surface's title (TS `updateTerminalTitle`): the cwd basename is TS
/// `path.basename` — empty for a root path.
pub(crate) fn session_title(session_name: Option<&str>, cwd: &Path) -> String {
    let cwd_basename = cwd
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    match session_name.filter(|name| !name.is_empty()) {
        Some(name) => format!("{APP_TITLE} - {name} - {cwd_basename}"),
        None => format!("{APP_TITLE} - {cwd_basename}"),
    }
}

/// The agents view's title (TS `AgentsViewMode`'s constructor).
pub(crate) fn agents_title() -> String {
    format!("{APP_TITLE} - Agents")
}

/// The process's title bookkeeping: whether the shell's title is saved on the stack, and the
/// title last written (an unchanged title writes nothing).
#[derive(Debug, Default)]
struct TitleState {
    pushed: bool,
    current: Option<String>,
}

impl TitleState {
    /// The bytes that set `title`: the stack push ahead of the process's first title, nothing
    /// when the title is already showing.
    fn set(&mut self, title: &str) -> Option<String> {
        let title: String = title.chars().filter(|ch| !ch.is_control()).collect();
        if self.current.as_deref() == Some(title.as_str()) {
            return None;
        }
        let mut bytes = String::new();
        if !self.pushed {
            bytes.push_str(PUSH_TITLE);
            self.pushed = true;
        }
        bytes.push_str("\x1b]0;");
        bytes.push_str(&title);
        bytes.push('\x07');
        self.current = Some(title);
        Some(bytes)
    }

    /// The bytes that hand the shell its title back: the stack pop, once per push.
    fn restore(&mut self) -> Option<&'static str> {
        self.current = None;
        std::mem::take(&mut self.pushed).then_some(POP_TITLE)
    }
}

static STATE: Mutex<TitleState> = Mutex::new(TitleState {
    pushed: false,
    current: None,
});

fn state() -> std::sync::MutexGuard<'static, TitleState> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Title the window (a no-op off a terminal: headless pipes never carry the title).
pub(crate) fn set(out: &mut Stdout, title: &str) {
    if !out.is_terminal() {
        return;
    }
    if let Some(bytes) = state().set(title) {
        let _ = out.write_all(bytes.as_bytes());
    }
}

/// Hand the shell its title back (the exit restore; a no-op when no title was set).
pub(crate) fn restore(out: &mut Stdout) {
    if let Some(bytes) = state().restore() {
        let _ = out.write_all(bytes.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_follow_the_ts_formats() {
        assert_eq!(
            [
                session_title(None, Path::new("/home/user/magic-journey")),
                session_title(Some("review"), Path::new("/home/user/magic-journey")),
                session_title(Some(""), Path::new("/home/user/magic-journey")),
                session_title(None, Path::new("/")),
                agents_title(),
            ],
            [
                "prime-agent - magic-journey".to_string(),
                "prime-agent - review - magic-journey".to_string(),
                "prime-agent - magic-journey".to_string(),
                "prime-agent - ".to_string(),
                "prime-agent - Agents".to_string(),
            ]
        );
    }

    #[test]
    fn the_first_title_saves_the_shell_title_and_the_exit_restores_it() {
        let mut state = TitleState::default();
        let emitted = [
            state.set("prime-agent - Agents"),
            state.set("prime-agent - magic-journey"),
            state.set("prime-agent - magic-journey"),
            state.set("prime-agent - review - magic-journey"),
        ];
        assert_eq!(
            emitted,
            [
                Some("\x1b[22;0t\x1b]0;prime-agent - Agents\x07".to_string()),
                Some("\x1b]0;prime-agent - magic-journey\x07".to_string()),
                None,
                Some("\x1b]0;prime-agent - review - magic-journey\x07".to_string()),
            ]
        );
        assert_eq!(
            [state.restore(), state.restore()],
            [Some("\x1b[23;0t"), None]
        );
        // A later surface in the same process saves the title again.
        assert_eq!(
            state.set("prime-agent - Agents"),
            Some("\x1b[22;0t\x1b]0;prime-agent - Agents\x07".to_string())
        );
    }

    #[test]
    fn control_characters_never_end_the_title_early() {
        let mut state = TitleState::default();
        assert_eq!(
            state.set("prime-agent - a\x07b\x1b]0;c - repo"),
            Some("\x1b[22;0t\x1b]0;prime-agent - ab]0;c - repo\x07".to_string())
        );
    }

    #[test]
    fn a_restore_without_a_title_writes_nothing() {
        assert_eq!(TitleState::default().restore(), None);
    }
}
