//! File permission policy. Unix: owner-only mode bits (0o600 files, 0o700
//! dirs). Windows: NTFS ACLs govern access - new files inherit ACLs from
//! their parent directory, so the restriction helpers are no-ops there.

use std::fs::OpenOptions;
use std::path::Path;

/// Owner-only file mode (Unix).
pub const PRIVATE_FILE_MODE: u32 = 0o600;
/// Owner-only directory mode (Unix).
pub const PRIVATE_DIR_MODE: u32 = 0o700;

/// Make a file owner-readable/writable only (`chmod 0o600`). Best-effort:
/// callers decide whether a failure is fatal.
///
/// # Errors
///
/// Returns the underlying I/O error when the permission change fails.
#[cfg(unix)]
pub fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

/// Windows arm of [`restrict_file`]: inherited ACLs carry the access
/// decision, so the restriction is a no-op.
///
/// # Errors
///
/// Does not error.
#[cfg(not(unix))]
pub fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Make a directory owner-accessible only (`chmod 0o700`).
///
/// # Errors
///
/// Returns the underlying I/O error when the permission change fails.
#[cfg(unix)]
pub fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(PRIVATE_DIR_MODE))
}

/// Windows arm of [`restrict_dir`]: inherited ACLs carry the access
/// decision, so the restriction is a no-op.
///
/// # Errors
///
/// Does not error.
#[cfg(not(unix))]
pub fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Set the private mode on files created through these options.
#[cfg(unix)]
pub fn set_private_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(PRIVATE_FILE_MODE);
}

#[cfg(not(unix))]
pub fn set_private_mode(_options: &mut OpenOptions) {
    // Windows: inherited ACLs apply; see the ACL note above.
}

/// The file's mode bits (`mode & 0o777`); None where mode bits do not exist.
#[cfg(unix)]
#[must_use]
pub fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
#[must_use]
pub fn file_mode(_path: &Path) -> Option<u32> {
    None
}

/// True when the path is an executable file (any execute bit on Unix).
#[cfg(unix)]
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// True when the current process can execute the file (`access(2)` `X_OK`
/// semantics: the uid/gid classes that apply to this process decide, not
/// just any execute bit in the mode). The mode-only [`is_executable`]
/// accepts a file whose only execute bit belongs to an unrelated group,
/// which this process would fail to spawn with permission denied.
#[cfg(unix)]
#[must_use]
pub fn is_executable_by_process(path: &Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

/// Windows arm of [`is_executable_by_process`]: NTFS ACLs carry the access
/// decision at `CreateProcess` time (no Unix execute classes exist), so the
/// resolution check stays the mode probe.
#[cfg(not(unix))]
#[must_use]
pub fn is_executable_by_process(path: &Path) -> bool {
    is_executable(path)
}

/// True when the current (real) user owns the file `metadata` describes,
/// so a file another user planted is never trusted (the prime CLI's
/// `st_uid == getuid()` rule for directory contexts).
#[cfg(unix)]
#[must_use]
pub fn is_owned_by_current_user(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.uid() == nix::unistd::Uid::current().as_raw()
}

/// Windows arm of [`is_owned_by_current_user`]: there is no uid to
/// compare (the prime CLI skips the check there too), so every file counts
/// as owned.
#[cfg(not(unix))]
#[must_use]
pub fn is_owned_by_current_user(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// Set the private mode on an already-open file (`fchmod`): exact bits despite
/// the umask; tightens a pre-existing loose file.
///
/// # Errors
///
/// The underlying I/O error when the permission bits cannot be set.
#[cfg(unix)]
pub fn restrict_open_file(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

/// Windows arm of [`restrict_open_file`]: inherited ACLs carry the
/// access decision, so the restriction is a no-op.
///
/// # Errors
///
/// Does not error.
#[cfg(not(unix))]
pub fn restrict_open_file(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// True when the path's owner is the effective user (Unix); an unreadable
/// path answers false. On Windows inherited ACLs govern access, so
/// ownership is not a separate gate and the check is a no-op true.
#[cfg(unix)]
#[must_use]
pub fn owned_by_effective_user(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.uid() == nix::unistd::Uid::effective().as_raw())
}

/// Windows arm of [`owned_by_effective_user`]: ownership cannot be
/// established here (inherited ACLs are not an ownership proof), so the
/// probe answers false and callers fail closed.
#[cfg(not(unix))]
#[must_use]
pub fn owned_by_effective_user(_path: &Path) -> bool {
    // Callers fail closed until a platform-proven private-ACL check exists.
    false
}

/// The effective user id (Unix); None where the probe does not exist, so
/// callers fail closed.
#[cfg(unix)]
#[must_use]
pub fn effective_uid() -> Option<u32> {
    Some(nix::unistd::Uid::effective().as_raw())
}

/// Windows arm of [`effective_uid`]: no uid-style owner probe exists, so
/// callers fail closed.
#[cfg(not(unix))]
#[must_use]
pub fn effective_uid() -> Option<u32> {
    None
}

/// Create directories recursively with the private dir mode on platforms with
/// mode bits; existing directories are left untouched (mkdir semantics).
///
/// # Errors
///
/// Returns the underlying I/O error when a directory cannot be created.
#[cfg(unix)]
pub fn create_dir_all_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(PRIVATE_DIR_MODE)
        .recursive(true)
        .create(path)
}

/// Windows arm of [`create_dir_all_private`]: inherited ACLs carry the
/// access decision, so the directories are plain recursive creates.
///
/// # Errors
///
/// Returns the underlying I/O error when a directory cannot be created.
#[cfg(not(unix))]
pub fn create_dir_all_private(path: &Path) -> std::io::Result<()> {
    // Windows: inherited ACLs apply; see the ACL note above.
    std::fs::create_dir_all(path)
}

#[cfg(all(test, unix))]
mod unix_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The process-access execute probe follows `access(2)`: our own
    /// executable file passes it, a mode without execute bits fails it, and
    /// a missing path is not executable. (The mode-bit-vs-process gap - a
    /// candidate executable only by an unrelated group - is pinned
    /// end-to-end by `pa-cli`'s `resolve_tailscale_binary` tests.)
    #[test]
    fn the_process_execute_probe_follows_access_semantics() {
        let guard = tempfile::Builder::new()
            .prefix("pa-perms-x-")
            .tempdir()
            .expect("temp dir");
        let dir = guard.path().to_path_buf();
        let file = dir.join("probe.sh");
        std::fs::write(&file, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(is_executable(&file));
        assert!(is_executable_by_process(&file));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(!is_executable(&file));
        assert!(!is_executable_by_process(&file));
        assert!(!is_executable_by_process(&dir.join("missing")));
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    /// The exact surface the private-journal contract enforces with: the
    /// effective-uid probe, the ownership probe (own paths true,
    /// unreadable paths false), the mode probe, private recursive
    /// creation, the tighten, and the private file-creation mode.
    #[test]
    fn ownership_mode_and_private_creation_probes() {
        let guard = tempfile::Builder::new()
            .prefix("pa-perms-")
            .tempdir()
            .expect("temp dir");
        let dir = guard.path().to_path_buf();

        assert!(effective_uid().is_some(), "unix has the uid probe");
        assert!(owned_by_effective_user(&dir), "an own directory is owned");
        assert!(
            !owned_by_effective_user(&dir.join("missing")),
            "an unreadable path answers false"
        );

        create_dir_all_private(&dir.join("nested/inner")).expect("create private");
        assert_eq!(
            file_mode(&dir.join("nested/inner")),
            Some(PRIVATE_DIR_MODE),
            "the created chain is private"
        );
        assert_eq!(file_mode(&dir.join("nested")), Some(PRIVATE_DIR_MODE));

        restrict_dir(&dir).expect("restrict");
        assert_eq!(file_mode(&dir), Some(PRIVATE_DIR_MODE));

        let file = dir.join("probe.ndjson");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        set_private_mode(&mut options);
        options
            .open(&file)
            .expect("open private")
            .sync_all()
            .expect("sync");
        assert_eq!(
            file_mode(&file),
            Some(PRIVATE_FILE_MODE),
            "the created file is private"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn restriction_is_a_no_op() {
        let guard = tempfile::Builder::new()
            .prefix("pa-perms-")
            .tempdir()
            .expect("temp dir");
        let dir = guard.path().to_path_buf();
        let file = dir.join("probe.txt");
        std::fs::write(&file, "x").expect("write");
        assert!(restrict_file(&file).is_ok());
        assert!(restrict_dir(&dir).is_ok());
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
    }

    /// The ownership probes fail closed: no ownership is claimed from
    /// inherited ACLs, and there is no uid-style probe on this platform.
    #[test]
    fn ownership_probes_fail_closed() {
        let guard = tempfile::Builder::new()
            .prefix("pa-perms-")
            .tempdir()
            .expect("temp dir");
        let dir = guard.path().to_path_buf();
        assert_eq!(effective_uid(), None);
        assert!(
            !owned_by_effective_user(&dir),
            "inherited ACLs are not an ownership proof"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
