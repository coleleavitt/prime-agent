//! The exec'd launcher: a confined (or session-leading) process starts as a
//! fresh process image that applies its own restriction and then execs the
//! real program, so the spawning host never forks.
//!
//! Why: a `pre_exec` hook makes std fork the host, and fork copies the
//! host's page tables, so every spawn costs time proportional to the host's
//! resident memory (3 to 5 ms at 120 MB, against 0.5 ms for a small host).
//! Without a hook std uses `posix_spawn` (a `vfork`-style clone), whose cost
//! does not grow with the host. The launcher then does in its own process
//! what the hook did between fork and exec: `setsid`, Landlock, seccomp,
//! all through safe APIs (no `unsafe`), before `exec`.
//!
//! The host registers its own executable as the launcher ([`set_launcher`]),
//! with a hidden first argument ([`LAUNCHER_FLAG`]) whose dispatch calls
//! [`launch_main`] before anything else starts. Without a registered
//! launcher (a library host such as a test binary) confinement falls back
//! to the in-process hook.
//!
//! The command line after the flag is
//! `[--setsid] [--ack-stdin] [--confine --network allowed|denied [--write ROOT]...] -- PROGRAM [ARG]...`.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use crate::policy::NetworkAccess;

/// The hidden first argument a host dispatches to [`launch_main`].
pub const LAUNCHER_FLAG: &str = "--prime-agent-sandbox-launch";

/// The byte the launcher writes to its stdin (a socket) once the child
/// leads its own session and is confined, right before `exec`: the spawner
/// may signal the child's group from then on.
pub const LAUNCH_ACK: u8 = 0x06;
/// The byte that precedes the launcher's error text on its stdin when it
/// cannot start the program (the text follows, then EOF).
pub const LAUNCH_NAK: u8 = 0x15;

/// How to start the launcher: a program and the arguments that precede the
/// launcher's own (the hidden flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launcher {
    program: PathBuf,
    leading_args: Vec<OsString>,
}

impl Launcher {
    #[must_use]
    pub fn new(program: PathBuf, leading_args: Vec<OsString>) -> Self {
        Self {
            program,
            leading_args,
        }
    }

    /// This process's own executable, invoked with [`LAUNCHER_FLAG`]. On
    /// Linux it is `/proc/self/exe`, which names the running image even
    /// after the file on disk is replaced (an update), so the launcher
    /// always speaks this binary's protocol.
    ///
    /// # Errors
    ///
    /// When the executable path cannot be determined.
    pub fn this_executable() -> std::io::Result<Self> {
        let program = if cfg!(target_os = "linux") {
            PathBuf::from("/proc/self/exe")
        } else {
            std::env::current_exe()?
        };
        Ok(Self::new(program, vec![LAUNCHER_FLAG.into()]))
    }

    /// A command running the launcher with `args` (its own flags, `--`,
    /// the program); the caller appends the program's arguments.
    fn command(&self, args: Vec<OsString>) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.leading_args).args(args);
        command
    }
}

static LAUNCHER: OnceLock<Launcher> = OnceLock::new();

/// Register the launcher every later confined or session-leading spawn of
/// this process goes through. The first registration wins.
///
/// # Errors
///
/// Returns `launcher` back when one was already registered.
pub fn set_launcher(launcher: Launcher) -> Result<(), Launcher> {
    LAUNCHER.set(launcher)
}

/// The registered launcher, if any.
#[must_use]
pub fn launcher() -> Option<&'static Launcher> {
    LAUNCHER.get()
}

/// The launcher flags of one spawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LaunchRequest {
    /// Lead a new session (no controlling terminal; the group id is the
    /// pid).
    pub(crate) setsid: bool,
    /// Write [`LAUNCH_ACK`] (or [`LAUNCH_NAK`] and the error) to stdin.
    pub(crate) ack_stdin: bool,
    /// Apply this Landlock + seccomp restriction (Linux).
    pub(crate) confine: Option<(Vec<PathBuf>, NetworkAccess)>,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}

impl LaunchRequest {
    /// The launcher's own arguments for this request, through the program
    /// (its arguments are appended by the caller).
    pub(crate) fn launcher_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if self.setsid {
            args.push("--setsid".into());
        }
        if self.ack_stdin {
            args.push("--ack-stdin".into());
        }
        if let Some((roots, network)) = &self.confine {
            args.push("--confine".into());
            args.push("--network".into());
            args.push(
                match network {
                    NetworkAccess::Allowed => "allowed",
                    NetworkAccess::Denied => "denied",
                }
                .into(),
            );
            for root in roots {
                args.push("--write".into());
                args.push(root.into());
            }
        }
        args.push("--".into());
        args.push(self.program.clone());
        args.extend(self.args.iter().cloned());
        args
    }

    pub(crate) fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut request = Self::default();
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut network = None;
        let mut confine = false;
        loop {
            let Some(arg) = args.next() else {
                return Err("missing `--` and the program".to_string());
            };
            match arg.to_str() {
                Some("--setsid") => request.setsid = true,
                Some("--ack-stdin") => request.ack_stdin = true,
                Some("--confine") => confine = true,
                Some("--network") => {
                    network = Some(match args.next().as_deref().and_then(OsStr::to_str) {
                        Some("allowed") => NetworkAccess::Allowed,
                        Some("denied") => NetworkAccess::Denied,
                        _ => return Err("--network takes `allowed` or `denied`".to_string()),
                    });
                }
                Some("--write") => match args.next() {
                    Some(root) => roots.push(PathBuf::from(root)),
                    None => return Err("--write takes a path".to_string()),
                },
                Some("--") => break,
                _ => return Err(format!("unknown launcher argument {}", arg.display())),
            }
        }
        request.program = args
            .next()
            .ok_or_else(|| "missing the program after `--`".to_string())?;
        request.args = args.collect();
        if confine {
            let network = network.ok_or_else(|| "--confine needs --network".to_string())?;
            request.confine = Some((roots, network));
        } else if network.is_some() || !roots.is_empty() {
            return Err("--network and --write need --confine".to_string());
        }
        Ok(request)
    }
}

/// A command running `program` through the registered launcher as
/// `request` says (the program's arguments are appended by the caller), or
/// `None` without a launcher.
pub(crate) fn launcher_command(request: &LaunchRequest) -> Option<Command> {
    launcher().map(|launcher| launcher.command(request.launcher_args()))
}

/// The launcher's entry point: `args` are the arguments after
/// [`LAUNCHER_FLAG`]. Applies the request to this process and execs the
/// program; returns only on failure, with the exit status to use (127,
/// after reporting the error on stdin when acknowledging, else on stderr).
#[must_use]
pub fn launch_main(args: impl IntoIterator<Item = OsString>) -> i32 {
    let request = match LaunchRequest::parse(args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("prime-agent sandbox launcher: {error}");
            return 127;
        }
    };
    let error = match prepare_self(&request) {
        Ok(()) => {
            if request.ack_stdin {
                // The spawner reads this before it may signal the group.
                let _ = write_stdin(&[LAUNCH_ACK]);
            }
            exec(&request)
        }
        Err(error) => {
            if request.ack_stdin {
                let mut message = vec![LAUNCH_NAK];
                message.extend_from_slice(error.as_bytes());
                if write_stdin(&message).is_ok() {
                    return 127;
                }
            }
            error
        }
    };
    eprintln!("prime-agent sandbox launcher: {error}");
    127
}

/// `setsid`, then the restriction: everything the old fork hook did.
fn prepare_self(request: &LaunchRequest) -> Result<(), String> {
    #[cfg(unix)]
    if request.setsid {
        rustix::process::setsid().map_err(|error| format!("setsid: {error}"))?;
    }
    #[cfg(not(unix))]
    if request.setsid {
        return Err("setsid is not supported on this platform".to_string());
    }
    if let Some((roots, network)) = &request.confine {
        restrict_self(roots, *network)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restrict_self(roots: &[PathBuf], network: NetworkAccess) -> Result<(), String> {
    crate::linux::restrict_self(roots, network).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn restrict_self(_roots: &[PathBuf], _network: NetworkAccess) -> Result<(), String> {
    Err("in-process confinement is Linux-only".to_string())
}

/// Write to stdin (the spawner's socket) through a duplicate descriptor:
/// std offers stdin only for reading.
#[cfg(unix)]
fn write_stdin(bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::fd::AsFd as _;
    let stdin = std::io::stdin().as_fd().try_clone_to_owned()?;
    std::fs::File::from(stdin).write_all(bytes)
}

#[cfg(not(unix))]
fn write_stdin(_bytes: &[u8]) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "no stdin acknowledgement on this platform",
    ))
}

/// Replace this process with the program; returns the error when that
/// fails.
#[cfg(unix)]
fn exec(request: &LaunchRequest) -> String {
    use std::os::unix::process::CommandExt as _;
    let error = Command::new(&request.program).args(&request.args).exec();
    format!("cannot run {}: {error}", request.program.display())
}

#[cfg(not(unix))]
fn exec(request: &LaunchRequest) -> String {
    format!(
        "cannot run {}: exec is not supported on this platform",
        request.program.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// Every request renders to launcher arguments that parse back to it.
    #[test]
    fn requests_round_trip_through_the_command_line() {
        let requests = [
            LaunchRequest {
                setsid: true,
                ack_stdin: true,
                confine: None,
                program: "/bin/sh".into(),
                args: os(&["-c", "true"]),
            },
            LaunchRequest {
                setsid: false,
                ack_stdin: false,
                confine: Some((
                    vec![PathBuf::from("/work space"), PathBuf::from("/tmp/--")],
                    NetworkAccess::Denied,
                )),
                program: "/usr/bin/python3".into(),
                args: os(&["-m", "rlm.repl", "--", "--setsid"]),
            },
            LaunchRequest {
                setsid: true,
                ack_stdin: false,
                confine: Some((Vec::new(), NetworkAccess::Allowed)),
                program: "--".into(),
                args: Vec::new(),
            },
        ];
        let parsed: Vec<Result<LaunchRequest, String>> = requests
            .iter()
            .map(|request| LaunchRequest::parse(request.launcher_args()))
            .collect();
        assert_eq!(parsed, requests.map(Ok).to_vec());
    }

    #[test]
    fn malformed_command_lines_are_refused() {
        let refused: Vec<Result<LaunchRequest, String>> = [
            os(&["--setsid"]),
            os(&["--"]),
            os(&["--bogus", "--", "x"]),
            os(&["--confine", "--", "x"]),
            os(&["--write", "/a", "--", "x"]),
            os(&["--confine", "--network", "maybe", "--", "x"]),
        ]
        .into_iter()
        .map(LaunchRequest::parse)
        .collect();
        assert_eq!(
            refused,
            vec![
                Err("missing `--` and the program".to_string()),
                Err("missing the program after `--`".to_string()),
                Err("unknown launcher argument --bogus".to_string()),
                Err("--confine needs --network".to_string()),
                Err("--network and --write need --confine".to_string()),
                Err("--network takes `allowed` or `denied`".to_string()),
            ]
        );
    }
}
