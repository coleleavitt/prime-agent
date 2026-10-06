//! UNIX related logic for terminal manipulation.

use crate::terminal::{
    sys::file_descriptor::{tty_fd, FileDesc},
    WindowSize,
};
#[cfg(feature = "libc")]
use libc::{
    cfmakeraw, ioctl, tcgetattr, tcsetattr, termios as Termios, winsize, STDOUT_FILENO, TCSANOW,
    TIOCGWINSZ,
};
use parking_lot::Mutex;
#[cfg(not(feature = "libc"))]
use rustix::{
    fd::AsFd,
    termios::{Termios, Winsize},
};

use std::{fs::File, io, process};
#[cfg(feature = "libc")]
use std::{
    mem,
    os::unix::io::{IntoRawFd, RawFd},
};

// Some(Termios) -> we're in the raw mode and this is the previous mode
// None -> we're not in the raw mode
static TERMINAL_MODE_PRIOR_RAW_MODE: Mutex<Option<Termios>> = parking_lot::const_mutex(None);

pub(crate) fn is_raw_mode_enabled() -> bool {
    TERMINAL_MODE_PRIOR_RAW_MODE.lock().is_some()
}

#[cfg(feature = "libc")]
impl From<winsize> for WindowSize {
    fn from(size: winsize) -> WindowSize {
        WindowSize {
            columns: size.ws_col,
            rows: size.ws_row,
            width: size.ws_xpixel,
            height: size.ws_ypixel,
        }
    }
}
#[cfg(not(feature = "libc"))]
impl From<Winsize> for WindowSize {
    fn from(size: Winsize) -> WindowSize {
        WindowSize {
            columns: size.ws_col,
            rows: size.ws_row,
            width: size.ws_xpixel,
            height: size.ws_ypixel,
        }
    }
}

#[allow(clippy::useless_conversion)]
#[cfg(feature = "libc")]
pub(crate) fn window_size() -> io::Result<WindowSize> {
    // http://rosettacode.org/wiki/Terminal_control/Dimensions#Library:_BSD_libc
    let mut size = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let file = File::open("/dev/tty").map(|file| (FileDesc::new(file.into_raw_fd(), true)));
    let fd = if let Ok(file) = &file {
        file.raw_fd()
    } else {
        // Fallback to libc::STDOUT_FILENO if /dev/tty is missing
        STDOUT_FILENO
    };

    if wrap_with_result(unsafe { ioctl(fd, TIOCGWINSZ.into(), &mut size) }).is_ok() {
        return Ok(size.into());
    }

    Err(std::io::Error::last_os_error().into())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn window_size() -> io::Result<WindowSize> {
    let file = File::open("/dev/tty").map(|file| (FileDesc::Owned(file.into())));
    let fd = if let Ok(file) = &file {
        file.as_fd()
    } else {
        // Fallback to libc::STDOUT_FILENO if /dev/tty is missing
        rustix::stdio::stdout()
    };
    let size = rustix::termios::tcgetwinsize(fd)?;
    Ok(size.into())
}

#[allow(clippy::useless_conversion)]
pub(crate) fn size() -> io::Result<(u16, u16)> {
    if let Ok(window_size) = window_size() {
        return Ok((window_size.columns, window_size.rows));
    }

    tput_size().ok_or_else(|| std::io::Error::last_os_error().into())
}

#[cfg(feature = "libc")]
pub(crate) fn enable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if original_mode.is_some() {
        return Ok(());
    }

    let tty = tty_fd()?;
    let fd = tty.raw_fd();
    let mut ios = get_terminal_attr(fd)?;
    let original_mode_ios = ios;
    raw_terminal_attr(&mut ios);
    set_terminal_attr(fd, &ios)?;
    // Keep it last - set the original mode only if we were able to switch to the raw mode
    *original_mode = Some(original_mode_ios);
    Ok(())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn enable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if original_mode.is_some() {
        return Ok(());
    }

    let tty = tty_fd()?;
    let mut ios = get_terminal_attr(&tty)?;
    let original_mode_ios = ios.clone();
    ios.make_raw();
    set_terminal_attr(&tty, &ios)?;
    // Keep it last - set the original mode only if we were able to switch to the raw mode
    *original_mode = Some(original_mode_ios);
    Ok(())
}

/// Reset the raw mode.
///
/// More precisely, reset the whole termios mode to what it was before the first call
/// to [enable_raw_mode]. If you don't mess with termios outside of crossterm, it's
/// effectively disabling the raw mode and doing nothing else.
#[cfg(feature = "libc")]
pub(crate) fn disable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if let Some(original_mode_ios) = original_mode.as_ref() {
        let tty = tty_fd()?;
        set_terminal_attr(tty.raw_fd(), original_mode_ios)?;
        // Keep it last - remove the original mode only if we were able to switch back
        *original_mode = None;
    }
    Ok(())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn disable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if let Some(original_mode_ios) = original_mode.as_ref() {
        let tty = tty_fd()?;
        set_terminal_attr(&tty, original_mode_ios)?;
        // Keep it last - remove the original mode only if we were able to switch back
        *original_mode = None;
    }
    Ok(())
}

#[cfg(not(feature = "libc"))]
fn get_terminal_attr(fd: impl AsFd) -> io::Result<Termios> {
    let result = rustix::termios::tcgetattr(fd)?;
    Ok(result)
}

#[cfg(not(feature = "libc"))]
fn set_terminal_attr(fd: impl AsFd, termios: &Termios) -> io::Result<()> {
    rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Now, termios)?;
    Ok(())
}

/// Prime Agent patch: the kitty graphics protocol's documented detection
/// query (a 1x1 RGB pixel, direct transmission, `a=q`: the terminal loads
/// and answers without storing it).
#[cfg(feature = "events")]
const GRAPHICS_QUERY: &[u8] = b"\x1B_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1B\\";

#[cfg(feature = "events")]
static GRAPHICS_QUERY_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Prime Agent patch: have the next keyboard enhancement support check also
/// ask whether the terminal speaks the kitty graphics protocol; its verdict
/// is read with [`crate::event::take_kitty_graphics_reply`]. Only for a
/// terminal reached directly: a multiplexer answers DA1 itself (and screen
/// can take an APC as a window title).
#[cfg(feature = "events")]
pub fn request_kitty_graphics_query() {
    GRAPHICS_QUERY_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Queries the terminal's support for progressive keyboard enhancement.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn supports_keyboard_enhancement() -> io::Result<bool> {
    if is_raw_mode_enabled() {
        read_supports_keyboard_enhancement_raw()
    } else {
        read_supports_keyboard_enhancement_flags()
    }
}

#[cfg(feature = "events")]
fn read_supports_keyboard_enhancement_flags() -> io::Result<bool> {
    enable_raw_mode()?;
    let flags = read_supports_keyboard_enhancement_raw();
    disable_raw_mode()?;
    flags
}

#[cfg(feature = "events")]
pub(crate) fn read_supports_keyboard_enhancement_raw() -> io::Result<bool> {
    use crate::event::{
        filter::KeyboardEnhancementFlagsFilter,
        poll_internal,
        read::{arm_capability_watch, lapse_capability_watch, take_capability_verdict},
        read_internal, InternalEvent,
    };
    use std::io::Write;
    use std::time::{Duration, Instant};

    // This is the recommended method for testing support for the keyboard enhancement protocol.
    // We send a query for the flags supported by the terminal and then the primary device attributes
    // query. If we receive the primary device attributes response but not the keyboard enhancement
    // flags, none of the flags are supported.
    //
    // See <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#detection-of-support-for-this-protocol>

    // ESC [ ? u        Query progressive keyboard enhancement flags (kitty protocol).
    // ESC [ c          Query primary device attributes.
    const KEYBOARD_QUERY: &[u8] = b"\x1B[?u\x1B[c";

    // Prime Agent patch: armed before the write, so the reply's verdict is
    // published by whichever poller parses it (see `read::ReplyWatch`).
    arm_capability_watch();
    // Prime Agent patch: a requested kitty graphics query rides ahead of
    // the keyboard query, concluded by the same DA1 (see
    // `read::arm_graphics_watch`).
    let graphics = GRAPHICS_QUERY_REQUESTED.swap(false, std::sync::atomic::Ordering::SeqCst);
    if graphics {
        crate::event::read::arm_graphics_watch();
    }
    let query: Vec<u8> = if graphics {
        [GRAPHICS_QUERY, KEYBOARD_QUERY].concat()
    } else {
        KEYBOARD_QUERY.to_vec()
    };
    let query = query.as_slice();

    let result = File::open("/dev/tty").and_then(|mut file| {
        file.write_all(query)?;
        file.flush()
    });
    if result.is_err() {
        let mut stdout = io::stdout();
        stdout.write_all(query)?;
        stdout.flush()?;
    }

    // Prime Agent v2: the same 250ms answer window, held in short slices
    // instead of one blocking hold. Each slice parks the shared event-reader
    // lock for at most a slice, so the app reader interleaves and delivers
    // early typing at its own cadence while the probe listens; a slice
    // timeout just yields to the app reader (an unanswered query keeps
    // waiting until the window deadline, like the 250ms hold it replaces).
    // The reply's verdict comes from the reply watch, published by whichever
    // poller parsed it — the probe's own slice, or the app reader's poll —
    // so the check never needs to win the reader lock to see a reply that
    // already arrived (diagnostic 1c5af0f for the window's origin).
    let deadline = Instant::now() + Duration::from_millis(250);
    let slice = Duration::from_millis(10);
    loop {
        if let Some(supported) = take_capability_verdict() {
            return Ok(supported);
        }
        let leftover = deadline.saturating_duration_since(Instant::now());
        if leftover.is_zero() {
            return lapse_capability_watch().map_or_else(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::Other,
                        "The keyboard enhancement status could not be read within a normal duration",
                    ))
                },
                Ok,
            );
        }
        // The poll drives the parse: a watched reply never reaches the
        // filter (its verdict ends the slice early, and the next turn takes
        // it). Only a reply that was already parked before the watch was
        // armed matches the filter here; it is read out as the verdict, the
        // upstream way. A lost lock round or a poll error just loops to the
        // verdict and deadline checks.
        match poll_internal(Some(leftover.min(slice)), &KeyboardEnhancementFlagsFilter) {
            Ok(true) => {
                let parked = read_internal(&KeyboardEnhancementFlagsFilter);
                let _ = lapse_capability_watch();
                return Ok(matches!(
                    parked,
                    Ok(InternalEvent::KeyboardEnhancementFlags(_))
                ));
            }
            Ok(false) | Err(_) => {
                // Yield the reader to the app between slices.
                std::thread::yield_now();
            }
        }
    }
}

/// execute tput with the given argument and parse
/// the output as a u16.
///
/// The arg should be "cols" or "lines"
fn tput_value(arg: &str) -> Option<u16> {
    let output = process::Command::new("tput").arg(arg).output().ok()?;
    let value = output
        .stdout
        .into_iter()
        .filter_map(|b| char::from(b).to_digit(10))
        .fold(0, |v, n| v * 10 + n as u16);

    if value > 0 {
        Some(value)
    } else {
        None
    }
}

/// Returns the size of the screen as determined by tput.
///
/// This alternate way of computing the size is useful
/// when in a subshell.
fn tput_size() -> Option<(u16, u16)> {
    match (tput_value("cols"), tput_value("lines")) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

#[cfg(feature = "libc")]
// Transform the given mode into an raw mode (non-canonical) mode.
fn raw_terminal_attr(termios: &mut Termios) {
    unsafe { cfmakeraw(termios) }
}

#[cfg(feature = "libc")]
fn get_terminal_attr(fd: RawFd) -> io::Result<Termios> {
    unsafe {
        let mut termios = mem::zeroed();
        wrap_with_result(tcgetattr(fd, &mut termios))?;
        Ok(termios)
    }
}

#[cfg(feature = "libc")]
fn set_terminal_attr(fd: RawFd, termios: &Termios) -> io::Result<()> {
    wrap_with_result(unsafe { tcsetattr(fd, TCSANOW, termios) })
}

#[cfg(feature = "libc")]
fn wrap_with_result(result: i32) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
