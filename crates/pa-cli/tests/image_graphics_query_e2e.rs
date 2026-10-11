//! Real-pty e2e for the kitty graphics query over ssh: an ssh session keeps
//! only `TERM` (no `KITTY_WINDOW_ID`, no `TERM_PROGRAM`), so TS's
//! environment detection says "no images"; the surface then asks the
//! terminal (`ESC _ G i=31,s=1,v=1,a=q,t=d,f=24;AAAA ESC \`, ahead of the
//! keyboard probe's `CSI ? u` + `CSI c`). A kitty reply places the
//! presented preview; a DA1-only terminal keeps the `[Image: …]` text; the
//! reply never reaches the key stream.
#![cfg(unix)]
#![allow(clippy::cast_possible_truncation)]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::fcntl::FcntlArg::F_SETFL;
use nix::fcntl::{OFlag, fcntl};
use nix::pty::{Winsize, openpty};

/// The graphics query the surface writes, then the keyboard probe's pair.
const PROBE: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[?u\x1b[c";
/// kitty's answers: the graphics `OK`, its keyboard flags, then DA1.
const KITTY_ANSWER: &[u8] = b"\x1b_Gi=31;OK\x1b\\\x1b[?0u\x1b[?62;c";
/// A terminal without either protocol answers only DA1.
const DA1_ANSWER: &[u8] = b"\x1b[?62;c";
/// The TS-parity transmit-and-place of the preview (kitty, direct).
const KITTY_TRANSMIT: &[u8] = b"\x1b_Ga=T,f=100,q=2";
/// A 16x9 red PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAJCAIAAAC0SDtlAAAAFUlEQVR4nGP4z8BAEiJN9agGWmkAACRlj3GqMTmoAAAAAElFTkSuQmCC";
const CHILD_SESSION_ENV: &str = "PA_IMAGE_QUERY_CHILD_SESSION";

/// The child half: the replay surface over a one-preview session.
#[test]
fn image_query_child_mode() {
    let Ok(session) = std::env::var(CHILD_SESSION_ENV) else {
        return;
    };
    let stream = pa_tui::session::JsonlSessionStream::from_path(std::path::Path::new(&session))
        .expect("the session parses");
    let options = pa_tui::app::AppOptions {
        auto_exit_ms: Some(8_000),
        ..pa_tui::app::AppOptions::default()
    };
    pa_tui::app::run_app(Box::new(stream), &options, Box::new(|_| {})).expect("the surface ran");
}

static HARNESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn a_kitty_reply_over_ssh_places_the_preview_without_leaking_keys() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut harness = Harness::start();
    harness.wait_for(0, PROBE, "the graphics query riding the keyboard probe");
    let mark = harness.output.len();
    harness.write(KITTY_ANSWER);
    harness.wait_for(mark, KITTY_TRANSMIT, "the preview's kitty placement");
    // The reply was consumed by the input path: a typed key paints, and no
    // byte of the reply ever reached the editor.
    let typed = harness.output.len();
    harness.write(b"Q");
    harness.wait_for(typed, b"Q", "the typed key's paint");
    let after = String::from_utf8_lossy(&harness.output[mark..]).into_owned();
    // `Gi=31` (not a bare `i=31`): the preview's own placement carries a
    // random image id (`_Ga=T,...,i=3188384900;...`) that a bare `i=31`
    // matches whenever the id starts with 31.
    for leaked in ["Gi=31", ";OK", "62;c"] {
        assert!(!after.contains(leaked), "{leaked:?} leaked:\n{after}");
    }
    harness.finish();
}

#[test]
fn a_da1_only_terminal_over_ssh_keeps_the_text_fallback() {
    let _lock = HARNESS_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut harness = Harness::start();
    harness.wait_for(0, PROBE, "the graphics query riding the keyboard probe");
    let mark = harness.output.len();
    harness.write(DA1_ANSWER);
    // The fallback row paints word by word (the cell diff skips blanks).
    harness.wait_for(mark, b"[Image:", "the textual fallback");
    harness.wait_for(mark, b"[image/png]", "the textual fallback's type");
    let typed = harness.output.len();
    harness.write(b"Q");
    harness.wait_for(typed, b"Q", "the typed key's paint");
    assert!(
        find(&harness.output, b"\x1b_Ga=").is_none(),
        "no image command on a terminal without the protocol"
    );
    harness.finish();
}

struct Harness {
    child: Child,
    master: std::fs::File,
    output: Vec<u8>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start() -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, session_jsonl()).expect("write the session");
        // 100x30 cells of 10x20 pixels: the TIOCGWINSZ cell size.
        let pty = openpty(
            Some(&Winsize {
                ws_row: 30,
                ws_col: 100,
                ws_xpixel: 1000,
                ws_ypixel: 600,
            }),
            None,
        )
        .expect("open pty");
        let child = spawn_child(&session, &pty.slave, dir.path());
        fcntl(pty.master.as_raw_fd(), F_SETFL(OFlag::O_NONBLOCK)).expect("non-blocking master");
        Self {
            child,
            master: pty.master.into(),
            output: Vec::new(),
            _dir: dir,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write to the pty");
    }

    fn wait_for(&mut self, from: usize, needle: &[u8], what: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if find(&self.output[from..], needle).is_some() {
                return;
            }
            let mut buffer = [0u8; 16384];
            match self.master.read(&mut buffer) {
                Ok(n) if n > 0 => self.output.extend_from_slice(&buffer[..n]),
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
            assert!(
                Instant::now() < deadline,
                "timeout waiting for {what}; pty tail:\n{}",
                String::from_utf8_lossy(&self.output[from..])
            );
        }
    }

    fn finish(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn session_jsonl() -> String {
    let ts = "2026-10-06T10:00:00.000Z";
    let rows = [
        serde_json::json!({"type": "session", "version": 3, "id": "image-query", "timestamp": ts, "cwd": "/tmp"}),
        serde_json::json!({"type": "message", "id": "e1", "parentId": null, "timestamp": ts,
            "message": {"role": "user", "content": [{"type": "text", "text": "show me"}], "timestamp": 0}}),
        serde_json::json!({"type": "custom_message", "id": "e2", "parentId": "e1", "timestamp": ts,
            "customType": "prime-agent.presented-artifact", "display": true,
            "content": [{"type": "text", "text": "Red"}, {"type": "image", "data": PNG, "mimeType": "image/png"}],
            "details": {"name": "red.png", "kind": "image", "mimeType": "image/png", "byteSize": 70,
                        "path": "/tmp/red.png", "width": 16, "height": 9,
                        "originalWidth": 16, "originalHeight": 9}}),
    ];
    rows.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn spawn_child(session: &std::path::Path, slave: &OwnedFd, home: &std::path::Path) -> Child {
    // Runs between fork and exec: become a session leader and claim the pty slave as the tty.
    fn claim_controlling_tty(fd: i32) -> std::io::Result<()> {
        nix::unistd::setsid()?;
        // SAFETY: TIOCSCTTY on the child's own pty slave fd, post-fork.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSCTTY as libc::c_ulong, 0) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    let slave_fd = slave.as_raw_fd();
    let stdio = || -> Stdio { slave.try_clone().expect("clone pty slave").into() };
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .arg("--exact")
        .arg("image_query_child_mode")
        .env(CHILD_SESSION_ENV, session)
        .env("HOME", home)
        // The ssh shape: TERM crosses (the pty request), nothing else does.
        .env("TERM", "xterm-kitty")
        .env("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22")
        .env("SSH_TTY", "/dev/pts/9")
        .env_remove("KITTY_WINDOW_ID")
        .env_remove("KITTY_PID")
        .env_remove("TERM_PROGRAM")
        .env_remove("GHOSTTY_RESOURCES_DIR")
        .env_remove("WEZTERM_PANE")
        .env_remove("ITERM_SESSION_ID")
        .env_remove("TMUX")
        .env_remove("STY")
        .env_remove("ZELLIJ")
        .stdin(stdio())
        .stdout(stdio())
        .stderr(stdio());
    // SAFETY: the pre_exec hook is the supported std seam for
    // session/terminal setup; it runs post-fork pre-exec in the child
    // only and cannot allocate.
    unsafe {
        command.pre_exec(move || claim_controlling_tty(slave_fd));
    }
    command.spawn().expect("spawn pty child")
}
