//! Owner-only files and directories (the TS `mode: 0o600` / `0o700`).

use std::io::Write as _;
use std::path::Path;

/// `mkdir -p` with mode `0700` for the directories it creates.
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Create (or truncate) `path` with mode `0600` and write `bytes`; with
/// `sync`, flush them to disk before returning.
pub(crate) fn write_private_file_synced(
    path: &Path,
    bytes: &[u8],
    exclusive: bool,
    sync: bool,
) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    if sync {
        file.sync_all()?;
    }
    Ok(())
}

/// Create (or truncate) `path` with mode `0600` and write `bytes`.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_private_file_synced(path, bytes, false, false)
}

/// Set `path`'s permission bits.
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// No mode bits off unix: the file keeps its ACL; only confirm it exists.
#[cfg(not(unix))]
pub(crate) fn set_mode(path: &Path, _mode: u32) -> std::io::Result<()> {
    std::fs::metadata(path).map(|_| ())
}
