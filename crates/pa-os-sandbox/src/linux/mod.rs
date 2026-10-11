//! Linux: Landlock for the filesystem (and TCP where the ABI has network
//! rules), seccomp for every non-unix socket when network is off.
//!
//! Everything that allocates (the Landlock ruleset, the BPF program) is
//! built here, in the parent, before the fork. The child only runs the
//! three syscalls in [`syscalls`], the one module of this crate allowed to
//! use `unsafe`.

#[allow(unsafe_code)] // the fork/exec hook and the ABI probe; see the module docs
mod syscalls;

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use landlock::{
    ABI,
    Access,
    AccessFs,
    AccessNet,
    CompatLevel,
    Compatible,
    PathBeneath,
    PathFd,
    Ruleset,
    RulesetAttr,
    RulesetCreatedAttr,
};

use crate::policy::NetworkAccess;
use crate::{Assessment, SandboxError};

/// The newest Landlock ABI whose rights this crate requests; older kernels
/// get the best-effort subset, and [`assess`] names what that loses.
const TARGET_ABI: ABI = ABI::V5;

/// Device files every confined process may open for writing (the null and
/// zero sinks, the controlling terminal, pseudo-terminal allocation).
const WRITABLE_DEVICE_FILES: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/tty",
    "/dev/ptmx",
];
/// Device directories a confined process may write beneath (its own
/// pseudo-terminals, POSIX shared memory for interpreter semaphores).
const WRITABLE_DEVICE_DIRS: &[&str] = &["/dev/pts", "/dev/shm"];

/// The running kernel's Landlock ABI version (`<= 0`: unavailable).
pub(crate) fn landlock_abi() -> i32 {
    syscalls::landlock_abi()
}

/// The seccomp architecture of this build (`None`: no socket filter).
#[cfg(target_arch = "x86_64")]
const SECCOMP_ARCH: Option<seccompiler::TargetArch> = Some(seccompiler::TargetArch::x86_64);
#[cfg(target_arch = "aarch64")]
const SECCOMP_ARCH: Option<seccompiler::TargetArch> = Some(seccompiler::TargetArch::aarch64);
#[cfg(target_arch = "riscv64")]
const SECCOMP_ARCH: Option<seccompiler::TargetArch> = Some(seccompiler::TargetArch::riscv64);
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
const SECCOMP_ARCH: Option<seccompiler::TargetArch> = None;

/// What a policy gets on a kernel with Landlock ABI `abi`: the mechanism
/// and every protection it cannot provide there. No Landlock at all is
/// unsupported: a sandbox without filesystem confinement would only
/// pretend.
pub(crate) fn assess(abi: i32, network: NetworkAccess) -> Result<Assessment, SandboxError> {
    if abi <= 0 {
        return Err(SandboxError::Unsupported {
            reason: "Landlock is not available in this Linux kernel (needs Linux 5.13+ with the \
                     `landlock` LSM enabled)"
                .to_string(),
        });
    }
    let mut gaps: Vec<String> = Vec::new();
    if abi < 3 {
        gaps.push(format!(
            "truncating existing files is not confined (Landlock ABI {abi} < 3)"
        ));
    }
    if abi < 5 {
        gaps.push(format!(
            "ioctl on device files is not confined (Landlock ABI {abi} < 5)"
        ));
    }
    let mut mechanism = format!("Landlock ABI {abi}");
    if network == NetworkAccess::Denied {
        if SECCOMP_ARCH.is_some() {
            mechanism.push_str(" + seccomp");
        } else if abi >= 4 {
            gaps.push("only TCP is blocked: UDP and raw sockets are not (no seccomp filter for this architecture)".to_string());
        } else {
            gaps.push(format!(
                "network is not blocked (Landlock ABI {abi} < 4 and no seccomp filter for this architecture)"
            ));
        }
    }
    Ok(Assessment { mechanism, gaps })
}

/// The prepared restriction: the Landlock ruleset descriptor (kept open in
/// the parent for every spawn of the command) and the socket filter.
pub(crate) struct LinuxSandbox {
    restriction: Arc<syscalls::Restriction>,
    roots: Vec<PathBuf>,
    network: NetworkAccess,
}

impl LinuxSandbox {
    /// Build the ruleset for `roots` (and, when network is off, the TCP
    /// rules and the seccomp program).
    pub(crate) fn prepare(roots: &[PathBuf], network: NetworkAccess) -> Result<Self, SandboxError> {
        let ruleset: Option<OwnedFd> = ruleset(roots, network)?.into();
        let ruleset = ruleset.ok_or_else(|| SandboxError::Unsupported {
            reason: "the kernel refused to create a Landlock ruleset".to_string(),
        })?;
        let socket_filter = match (network, SECCOMP_ARCH) {
            (NetworkAccess::Denied, Some(arch)) => Some(
                socket_filter(arch)?
                    .into_iter()
                    .map(|instruction| libc::sock_filter {
                        code: instruction.code,
                        jt: instruction.jt,
                        jf: instruction.jf,
                        k: instruction.k,
                    })
                    .collect(),
            ),
            (NetworkAccess::Denied, None) | (NetworkAccess::Allowed, _) => None,
        };
        Ok(Self {
            restriction: Arc::new(syscalls::Restriction::new(ruleset, socket_filter)),
            roots: roots.to_vec(),
            network,
        })
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub(crate) fn network(&self) -> NetworkAccess {
        self.network
    }

    /// Confine `command`'s child between fork and exec.
    pub(crate) fn apply(&self, command: &mut Command) {
        syscalls::confine_on_exec(command, Arc::clone(&self.restriction));
    }
}

fn setup_error(context: &'static str, error: impl std::fmt::Display) -> SandboxError {
    SandboxError::Setup {
        context,
        message: error.to_string(),
    }
}

/// Confine this process (the launcher, before it execs the program) with
/// the same rules [`LinuxSandbox::prepare`] builds, through landlock's and
/// seccompiler's safe self-restriction calls. A root that no longer exists
/// is skipped: it cannot be written either way.
pub(crate) fn restrict_self(roots: &[PathBuf], network: NetworkAccess) -> Result<(), SandboxError> {
    let existing: Vec<PathBuf> = roots.iter().filter(|root| root.exists()).cloned().collect();
    let status = ruleset(&existing, network)?
        .restrict_self()
        .map_err(|error| setup_error("Landlock restrict_self", error))?;
    if status.ruleset == landlock::RulesetStatus::NotEnforced {
        return Err(SandboxError::Unsupported {
            reason: "the kernel did not enforce the Landlock ruleset".to_string(),
        });
    }
    if let (NetworkAccess::Denied, Some(arch)) = (network, SECCOMP_ARCH) {
        seccompiler::apply_filter(&socket_filter(arch)?)
            .map_err(|error| setup_error("seccomp filter", error))?;
    }
    Ok(())
}

/// Everything readable and executable; writes only beneath `roots` and the
/// device allowlist; no TCP when network is off.
fn ruleset(
    roots: &[PathBuf],
    network: NetworkAccess,
) -> Result<landlock::RulesetCreated, SandboxError> {
    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(TARGET_ABI))
        .map_err(|error| setup_error("Landlock filesystem rights", error))?;
    if network == NetworkAccess::Denied {
        // No TCP rule follows: every bind and connect is refused.
        ruleset = ruleset
            .handle_access(AccessNet::from_all(TARGET_ABI))
            .map_err(|error| setup_error("Landlock network rights", error))?;
    }
    let mut created = ruleset
        .create()
        .map_err(|error| setup_error("Landlock ruleset", error))?;
    created = add_rule(created, Path::new("/"), AccessFs::from_read(TARGET_ABI))?;
    for root in roots {
        created = add_rule(created, root, AccessFs::from_all(TARGET_ABI))?;
    }
    for device in WRITABLE_DEVICE_FILES {
        let path = Path::new(device);
        if path.exists() {
            created = add_rule(created, path, AccessFs::from_file(TARGET_ABI))?;
        }
    }
    for directory in WRITABLE_DEVICE_DIRS {
        let path = Path::new(directory);
        if path.is_dir() {
            created = add_rule(created, path, AccessFs::from_all(TARGET_ABI))?;
        }
    }
    Ok(created)
}

fn add_rule(
    ruleset: landlock::RulesetCreated,
    path: &Path,
    access: landlock::BitFlags<AccessFs>,
) -> Result<landlock::RulesetCreated, SandboxError> {
    let descriptor = PathFd::new(path).map_err(|error| SandboxError::Setup {
        context: "Landlock path",
        message: format!("{}: {error}", path.display()),
    })?;
    ruleset
        .add_rule(PathBeneath::new(descriptor, access))
        .map_err(|error| SandboxError::Setup {
            context: "Landlock rule",
            message: format!("{}: {error}", path.display()),
        })
}

/// `socket(2)` outside `AF_UNIX`, and the `io_uring` syscalls (which can open
/// sockets without `socket(2)`), fail with `EPERM`; everything else passes.
fn socket_filter(arch: seccompiler::TargetArch) -> Result<seccompiler::BpfProgram, SandboxError> {
    use seccompiler::{
        SeccompAction,
        SeccompCmpArgLen,
        SeccompCmpOp,
        SeccompCondition,
        SeccompFilter,
        SeccompRule,
    };
    let filter_error = |error: seccompiler::BackendError| setup_error("seccomp filter", error);
    let not_unix = SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Ne,
        libc::AF_UNIX as u64,
    )
    .map_err(filter_error)?;
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(
        libc::SYS_socket,
        vec![SeccompRule::new(vec![not_unix]).map_err(filter_error)?],
    );
    for syscall in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        // An empty rule list matches the syscall unconditionally.
        rules.insert(syscall, Vec::new());
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(filter_error)?;
    filter.try_into().map_err(filter_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_assessment_names_every_protection_an_old_abi_lacks() {
        let full = assess(6, NetworkAccess::Denied).unwrap();
        let abi_two = assess(2, NetworkAccess::Allowed).unwrap();
        assert_eq!(
            (full, abi_two),
            (
                Assessment {
                    mechanism: "Landlock ABI 6 + seccomp".to_string(),
                    gaps: Vec::new(),
                },
                Assessment {
                    mechanism: "Landlock ABI 2".to_string(),
                    gaps: vec![
                        "truncating existing files is not confined (Landlock ABI 2 < 3)"
                            .to_string(),
                        "ioctl on device files is not confined (Landlock ABI 2 < 5)".to_string(),
                    ],
                },
            )
        );
    }

    #[test]
    fn no_landlock_is_unsupported_not_degraded() {
        assert!(matches!(
            assess(0, NetworkAccess::Allowed),
            Err(SandboxError::Unsupported { .. })
        ));
        assert!(matches!(
            assess(-38, NetworkAccess::Denied),
            Err(SandboxError::Unsupported { .. })
        ));
    }

    #[test]
    fn the_socket_filter_compiles_for_this_architecture() {
        let arch = SECCOMP_ARCH.expect("a seccomp architecture");
        assert!(!socket_filter(arch).unwrap().is_empty());
    }
}
