//! macOS Seatbelt: the SBPL profile `/usr/bin/sandbox-exec` enforces (the
//! approach the Codex CLI uses). Profile generation is pure and compiled on
//! every platform so its unit tests run everywhere; only the launcher is
//! macOS-only.
//!
//! The writable roots never appear in the profile text: each is a
//! `(param "WRITABLE_ROOT_<n>")` reference bound with `-D` on the command
//! line, so a path cannot inject SBPL.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::policy::NetworkAccess;

/// The launcher every confined process runs under.
pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The fixed part: deny by default, read everything, run and signal
/// processes inside the sandbox, and use the terminal and the system
/// services an interpreter needs. Writes and network are added per policy.
const BASE_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow file-read*)
(allow file-write-data
  (literal "/dev/null")
  (literal "/dev/zero")
  (literal "/dev/dtracehelper")
  (literal "/dev/tty")
  (regex #"^/dev/ttys[0-9]+$"))
(allow file-ioctl
  (literal "/dev/dtracehelper")
  (literal "/dev/tty")
  (regex #"^/dev/ttys[0-9]+$"))
(allow pseudo-tty)
(allow sysctl-read)
(allow mach-lookup)
(allow ipc-posix-sem)
(allow ipc-posix-shm*)
(allow user-preference-read)
"#;

/// The profile for one launch with `root_count` writable roots.
pub(crate) fn profile(root_count: usize, network: NetworkAccess) -> String {
    let mut profile = BASE_PROFILE.to_string();
    if root_count > 0 {
        profile.push_str("(allow file-write*");
        for index in 0..root_count {
            // Writing into a String cannot fail.
            let _ = write!(profile, "\n  (subpath (param \"WRITABLE_ROOT_{index}\"))");
        }
        profile.push_str(")\n");
    }
    match network {
        NetworkAccess::Allowed => {
            profile.push_str(
                "(allow network-outbound)\n(allow network-inbound)\n(allow system-socket)\n",
            );
        }
        NetworkAccess::Denied => {
            // Local unix sockets stay usable (the system log, the resolver
            // daemon's socket is not reachable without inet anyway).
            profile.push_str(
                "(allow network-outbound (remote unix-socket))\n(allow system-socket (socket-domain AF_UNIX))\n",
            );
        }
    }
    profile
}

/// The `sandbox-exec` arguments that precede the confined program:
/// `-p <profile> -D WRITABLE_ROOT_<n>=<path>... --`.
pub(crate) fn launcher_args(roots: &[PathBuf], network: NetworkAccess) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-p".into(), profile(roots.len(), network).into()];
    for (index, root) in roots.iter().enumerate() {
        let mut binding = OsString::from(format!("WRITABLE_ROOT_{index}="));
        binding.push(root.as_os_str());
        args.push("-D".into());
        args.push(binding);
    }
    args.push("--".into());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_roots_are_parameters_and_network_off_keeps_only_unix_sockets() {
        let roots = vec![
            PathBuf::from("/Users/me/project"),
            PathBuf::from("/private/var/folders/x\")(allow default"),
        ];
        let args = launcher_args(&roots, NetworkAccess::Denied);
        let expected_profile = format!(
            "{BASE_PROFILE}(allow file-write*\n  (subpath (param \"WRITABLE_ROOT_0\"))\n  \
             (subpath (param \"WRITABLE_ROOT_1\")))\n\
             (allow network-outbound (remote unix-socket))\n\
             (allow system-socket (socket-domain AF_UNIX))\n"
        );
        let expected: Vec<OsString> = vec![
            "-p".into(),
            expected_profile.into(),
            "-D".into(),
            "WRITABLE_ROOT_0=/Users/me/project".into(),
            "-D".into(),
            "WRITABLE_ROOT_1=/private/var/folders/x\")(allow default".into(),
            "--".into(),
        ];
        assert_eq!(args, expected);
    }

    #[test]
    fn network_on_allows_sockets_and_no_roots_adds_no_write_rule() {
        assert_eq!(
            profile(0, NetworkAccess::Allowed),
            format!(
                "{BASE_PROFILE}(allow network-outbound)\n(allow network-inbound)\n(allow system-socket)\n"
            )
        );
    }

    #[test]
    fn the_profile_denies_by_default_and_never_allows_writes_outright() {
        let profile = profile(1, NetworkAccess::Denied);
        assert!(profile.starts_with("(version 1)\n(deny default)\n"));
        // Every write grant is scoped: device literals or a root parameter.
        assert!(!profile.contains("(allow file-write*)"));
        assert!(!profile.contains("(allow network*)"));
    }
}
