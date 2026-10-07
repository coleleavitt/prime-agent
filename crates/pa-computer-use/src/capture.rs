//! The hardened screenshot directory every backend captures into.
//!
//! `<agent dir>/tmp/computer-use/`, opened through a no-follow component
//! chain below the user's home (a symlink planted into the state tree after
//! the check cannot redirect the sweep, the mode change or the write),
//! private (0700, PNGs 0600, best-effort), swept before and after each
//! capture (files older than 24 hours go, at most the 20 most recent stay),
//! and every written PNG is read back through the verified descriptor.

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::error::{head, transport, ComputerUseError, Result, ERROR_LIMIT};

const SWEEP_MAX_FILES: usize = 20;
const SWEEP_MAX_AGE_SECONDS: f64 = 24.0 * 60.0 * 60.0;

fn os_text(errno: Errno) -> String {
    std::io::Error::from(errno).to_string()
}

fn unavailable(errno: Errno) -> ComputerUseError {
    transport(format!(
        "screenshot directory unavailable: {}",
        head(&os_text(errno), ERROR_LIMIT)
    ))
}

fn symlinked(component: &dyn std::fmt::Display) -> ComputerUseError {
    transport(format!(
        "screenshot path component is a symlink: {component}"
    ))
}

/// Where captures go, and the home the no-follow chain starts from.
#[derive(Debug, Clone)]
pub(crate) struct CaptureDir {
    dir: PathBuf,
    home: PathBuf,
    #[cfg(test)]
    fixed_name: Option<String>,
}

impl CaptureDir {
    /// `<agent_dir>/tmp/computer-use`, chained from `$HOME`.
    pub(crate) fn under_agent_dir(agent_dir: &Path) -> Self {
        Self::new(
            agent_dir.join("tmp").join("computer-use"),
            std::env::home_dir().unwrap_or_default(),
        )
    }

    pub(crate) fn new(dir: PathBuf, home: PathBuf) -> Self {
        Self {
            dir,
            home,
            #[cfg(test)]
            fixed_name: None,
        }
    }

    /// Name the next capture `name` instead of a random one (tests plant
    /// files at the exact target).
    #[cfg(test)]
    pub(crate) fn with_fixed_name(mut self, name: &str) -> Self {
        self.fixed_name = Some(name.to_string());
        self
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.dir
    }

    /// Refuse a directory that is, or below the home contains, a symlink;
    /// then open it through the no-follow chain and sweep it.
    pub(crate) fn open(&self) -> Result<OpenCaptureDir<'_>> {
        self.refuse_symlinked()?;
        let fd = self.open_chain()?;
        let open = OpenCaptureDir { dir: self, fd };
        open.sweep(None);
        Ok(open)
    }

    /// Components below the home are all checked (the state tree is
    /// plantable); a path outside the home checks only itself, like the
    /// macOS system symlinks `/var` and `/tmp`.
    fn refuse_symlinked(&self) -> Result<()> {
        let mut component: &Path = &self.dir;
        while component != self.home {
            let Some(parent) = component.parent() else {
                return Ok(());
            };
            if component.is_symlink() {
                return Err(symlinked(&component.display()));
            }
            if !component.starts_with(&self.home) || component == self.home {
                return Ok(());
            }
            component = parent;
        }
        Ok(())
    }

    fn open_chain(&self) -> Result<OwnedFd> {
        let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let Ok(below_home) = self.dir.strip_prefix(&self.home) else {
            // Outside the home: keep following system symlinks.
            if self.dir.is_symlink() {
                return Err(symlinked(&self.dir.display()));
            }
            std::fs::create_dir_all(&self.dir).map_err(|error| {
                transport(format!(
                    "screenshot directory unavailable: {}",
                    head(&error.to_string(), ERROR_LIMIT)
                ))
            })?;
            let fd = rustix::fs::open(&self.dir, directory, Mode::empty()).map_err(unavailable)?;
            let _ = rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700));
            return Ok(fd);
        };
        let mut fd = rustix::fs::open(&self.home, directory, Mode::empty()).map_err(unavailable)?;
        for component in below_home.components() {
            fd = open_component(&fd, component.as_os_str())?;
        }
        let _ = rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700));
        Ok(fd)
    }
}

/// Open one component under the chain, creating it 0700 when missing.
/// `O_NOFOLLOW` refuses a symlinked component at open time: the check that
/// cannot be raced between validation and use.
fn open_component(dir: &OwnedFd, component: &std::ffi::OsStr) -> Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match rustix::fs::openat(dir, component, flags, Mode::empty()) {
        Ok(fd) => Ok(fd),
        Err(Errno::NOENT) => {
            match rustix::fs::mkdirat(dir, component, Mode::from_raw_mode(0o700)) {
                // A concurrent capture created it first; open it below.
                Ok(()) | Err(Errno::EXIST) => {}
                Err(errno) => return Err(unavailable(errno)),
            }
            rustix::fs::openat(dir, component, flags, Mode::empty()).map_err(unavailable)
        }
        // Linux reports ENOTDIR for O_NOFOLLOW|O_DIRECTORY on a symlink, macOS ELOOP.
        Err(errno @ (Errno::LOOP | Errno::NOTDIR)) => {
            let is_link = rustix::fs::statat(dir, component, AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Symlink);
            if is_link {
                return Err(symlinked(&component.to_string_lossy()));
            }
            Err(unavailable(errno))
        }
        Err(errno) => Err(unavailable(errno)),
    }
}

/// The capture directory, open through its verified descriptor.
pub(crate) struct OpenCaptureDir<'a> {
    dir: &'a CaptureDir,
    fd: OwnedFd,
}

impl OpenCaptureDir<'_> {
    /// A fresh capture target: a random `<uuid>.png`, refused when something
    /// that is not a regular file already sits there.
    pub(crate) fn new_target(&self) -> Result<(String, PathBuf)> {
        #[cfg(test)]
        let name = self
            .dir
            .fixed_name
            .clone()
            .unwrap_or_else(|| format!("{}.png", uuid::Uuid::new_v4()));
        #[cfg(not(test))]
        let name = format!("{}.png", uuid::Uuid::new_v4());
        match rustix::fs::statat(&self.fd, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Err(Errno::NOENT) => {}
            Err(errno) => return Err(unavailable(errno)),
            Ok(stat) => {
                if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                    return Err(transport(format!(
                        "screenshot target path is not a regular file: {name}"
                    )));
                }
            }
        }
        let path = self.dir.dir.join(&name);
        Ok((name, path))
    }

    /// Owner-only, best-effort, through the descriptor (never a symlink).
    pub(crate) fn make_private(&self, name: &str) {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        if let Ok(file) = rustix::fs::openat(&self.fd, name, flags, Mode::empty()) {
            let regular = rustix::fs::fstat(&file)
                .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile);
            if regular {
                let _ = rustix::fs::fchmod(&file, Mode::from_raw_mode(0o600));
            }
        }
    }

    /// The IHDR size of the written PNG, read through the verified
    /// descriptor without following a swapped-in symlink or blocking on a
    /// planted FIFO.
    pub(crate) fn png_dimensions(&self, name: &str) -> Result<(u32, u32)> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let unreadable = |errno: Errno| {
            transport(format!(
                "screencapture did not write a readable PNG: {}",
                head(&os_text(errno), ERROR_LIMIT)
            ))
        };
        let file = rustix::fs::openat(&self.fd, name, flags, Mode::empty()).map_err(unreadable)?;
        let stat = rustix::fs::fstat(&file).map_err(unreadable)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(transport(format!(
                "screencapture did not write a regular PNG: {name}"
            )));
        }
        let mut header = [0_u8; 24];
        let mut filled = 0;
        while filled < header.len() {
            match rustix::io::read(&file, &mut header[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(Errno::INTR) => {}
                Err(errno) => return Err(unreadable(errno)),
            }
        }
        if filled < 24 || &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
            return Err(transport(format!(
                "screencapture did not write a valid PNG: {name}"
            )));
        }
        let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
        let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
        if width < 1 || height < 1 {
            return Err(transport(format!(
                "screencapture wrote an empty PNG: {name}"
            )));
        }
        Ok((width, height))
    }

    /// Delete files older than 24 hours, then keep only the 20 most recent,
    /// best-effort and through the descriptor only (an unlink removes a
    /// planted entry itself, never its target). `keep` is never removed and
    /// counts toward the cap, so the capture just written survives even when
    /// older files carry future timestamps.
    pub(crate) fn sweep(&self, keep: Option<&str>) {
        let Ok(listing) = rustix::fs::Dir::read_from(&self.fd) else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |elapsed| elapsed.as_secs_f64());
        let mut entries: Vec<(f64, String)> = Vec::new();
        for entry in listing.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            if let Ok(stat) =
                rustix::fs::statat(self.fd.as_fd(), name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            {
                #[allow(clippy::cast_precision_loss)] // file timestamps fit an f64's mantissa
                let modified = stat.st_mtime as f64
                    + f64::from(u32::try_from(stat.st_mtime_nsec).unwrap_or(0)) / 1e9;
                entries.push((modified, name));
            }
        }
        entries.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
        let mut kept = usize::from(keep.is_some());
        for (modified, name) in entries {
            if Some(name.as_str()) == keep {
                continue;
            }
            if now - modified > SWEEP_MAX_AGE_SECONDS || kept >= SWEEP_MAX_FILES {
                let _ = rustix::fs::unlinkat(&self.fd, name.as_str(), AtFlags::empty());
            } else {
                kept += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests;
