//! OSC 8 under tmux (upstream #876): tmux 3.4+ forwards hyperlinks when its client negotiated
//! the `hyperlinks` terminal feature, which `#{client_termfeatures}` reports. The gate asks a
//! fake `tmux` on `PATH` (its own test binary: the env and the cached probe are process-global).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

#[test]
fn tmux_advertising_hyperlinks_enables_osc8() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let tmux = dir.path().join("tmux");
    let log = dir.path().join("args");
    std::fs::write(
        &tmux,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf '256,RGB,bpaste,clipboard,hyperlinks\\n'\n",
            log.display()
        ),
    )
    .expect("write fake tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", dir.path().display()));
    std::env::set_var("TMUX", "/tmp/tmux-test/default,1234,0");
    std::env::set_var("TMUX_PANE", "%7");
    std::env::set_var("TERM", "tmux-256color");
    std::env::remove_var("TERM_PROGRAM");

    assert!(pa_tui::hyperlinks::hyperlinks_enabled());
    // The probe runs once per process; later calls read the cached answer.
    assert!(pa_tui::hyperlinks::hyperlinks_enabled());
    let calls = std::fs::read_to_string(&log).expect("the probe ran");
    assert_eq!(calls, "display-message -p -t %7 #{client_termfeatures}\n");
}
