//! pa-core platform wall: every OS-specific behavior behind small traits in
//! cfg-gated modules. Unix and Windows implementations live behind the same
//! signatures; session-engine call sites never branch on `cfg` themselves.

pub mod browser;
pub mod fs;
pub mod lock_dir;
pub mod perms;
pub mod private_fs;
pub mod process;
pub mod process_tree;
pub mod shell;
pub mod sync_dir;

pub use fs::fsync;
pub use lock_dir::{HeartbeatLock, LockDir, LockHolder, lock_exclusive, try_lock_exclusive};
// The rename-onto-destination primitive (bounded win32 destination-busy
// retry) lives in pa-telemetry — the bottom crate every persist owner
// already depends on; re-exported so the platform wall stays the engine's
// single platform entry.
pub use pa_telemetry::rename_onto;
pub use perms::{
    file_mode,
    is_executable,
    is_executable_by_process,
    is_owned_by_current_user,
    is_readable_writable,
    restrict_dir,
    restrict_file,
    set_private_mode,
};
pub use process::{
    Signal,
    kill_pid,
    kill_process_group,
    kill_process_group_or_pid,
    pid_exists,
    process_group_exists,
    set_new_process_group,
    set_no_window,
    signal_process_group,
    termination_signal,
};
pub use shell::{ShellConfig, get_shell_config, resolve_kernel_bash_shell};
pub use sync_dir::sync_dir;
