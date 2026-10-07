//! The Linux session lock, shared by the X11 and Wayland backends: logind's
//! `LockedHint`, which niri and the common X11 lockers maintain.

use crate::process::{optional_tool, run_tool, Tools, TOOL_TIMEOUT};

/// Whether the session is locked, failing closed: a missing `loginctl`, a
/// failed read, unparsable output, or a session that is not `Active`
/// (another VT) all read as locked.
pub(crate) fn screen_locked(tools: &dyn Tools, absolute_loginctl: Option<&str>) -> bool {
    let Some(loginctl) = optional_tool(tools, "loginctl", absolute_loginctl) else {
        return true;
    };
    let session = tools
        .env("XDG_SESSION_ID")
        .filter(|session| !session.is_empty())
        .unwrap_or_else(|| "auto".to_string());
    let argv: Vec<String> = [
        loginctl.as_str(),
        "show-session",
        &session,
        "-p",
        "LockedHint",
        "-p",
        "Active",
    ]
    .into_iter()
    .map(ToString::to_string)
    .collect();
    let Ok(output) = run_tool(tools, &argv, TOOL_TIMEOUT) else {
        return true;
    };
    if !output.success() {
        return true;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value = |key: &str| {
        text.lines()
            .rev()
            .filter_map(|line| line.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    };
    value("LockedHint") != Some("no") || value("Active") != Some("yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::script::Script;

    fn lock(stdout: &[u8], code: i32, tools: &[&str]) -> (bool, Script) {
        let script = Script::with_tools(tools);
        script.set_env("XDG_SESSION_ID", "2");
        script.on(&["show-session"], code, stdout, b"");
        (screen_locked(&script, None), script)
    }

    #[test]
    fn the_locked_hint_parses_and_fails_closed() {
        let (locked, script) = lock(b"LockedHint=no\nActive=yes\n", 0, &["loginctl"]);
        assert!(!locked);
        assert_eq!(
            script.calls(),
            [[
                "/usr/bin/loginctl",
                "show-session",
                "2",
                "-p",
                "LockedHint",
                "-p",
                "Active"
            ]]
        );
        assert!(lock(b"LockedHint=yes\nActive=yes\n", 0, &["loginctl"]).0);
        assert!(lock(b"LockedHint=no\nActive=no\n", 0, &["loginctl"]).0);
        assert!(lock(b"", 1, &["loginctl"]).0);
        assert!(lock(b"garbage", 0, &["loginctl"]).0);
        assert!(lock(b"LockedHint=no\nActive=yes\n", 0, &[]).0);
    }

    #[test]
    fn without_a_session_id_the_auto_session_is_read() {
        let script = Script::with_tools(&["loginctl"]);
        script.on(&["show-session"], 0, b"LockedHint=no\nActive=yes\n", b"");
        assert!(!screen_locked(&script, None));
        assert_eq!(script.calls()[0][2], "auto");
    }
}
