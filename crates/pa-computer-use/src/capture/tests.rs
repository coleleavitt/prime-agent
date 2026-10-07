//! Ported from the skill's `tests/test_capture_security.py` (the directory,
//! sweep, symlink, FIFO, mode and PNG-hygiene halves; the screencapture argv
//! and error halves are in the macOS backend's tests).

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::*;
use crate::error::ErrorCode;
use crate::process::script::png_bytes;

const STALE: Duration = Duration::from_mins(24 * 60 + 1);

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn write_shot(dir: &Path, name: &str, age: Duration) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, b"png").unwrap();
    let stamp = SystemTime::now() - age;
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    path
}

/// One capture: open, take a target, "write" a PNG, verify, sweep.
fn capture(dir: &CaptureDir, png: &[u8]) -> Result<(std::path::PathBuf, (u32, u32))> {
    let open = dir.open()?;
    let (name, path) = open.new_target()?;
    std::fs::write(&path, png).unwrap();
    open.make_private(&name);
    let size = open.png_dimensions(&name)?;
    open.sweep(Some(&name));
    Ok((path, size))
}

fn outside_home(tmp: &Path) -> CaptureDir {
    CaptureDir::new(tmp.join("shots"), tmp.join("unrelated-home"))
}

#[test]
fn the_capture_dir_and_the_png_are_private() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    let (path, size) = capture(&dir, &png_bytes(5, 5)).unwrap();
    assert_eq!(size, (5, 5));
    assert_eq!(mode(dir.path()), 0o700);
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn the_sweep_deletes_files_older_than_a_day() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    let stale = write_shot(dir.path(), "stale.png", STALE);
    let fresh = write_shot(dir.path(), "fresh.png", Duration::from_secs(60));
    capture(&dir, &png_bytes(5, 5)).unwrap();
    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[test]
fn the_sweep_keeps_the_twenty_most_recent_counting_the_new_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    let paths: Vec<_> = (0..25)
        .map(|index| {
            write_shot(
                dir.path(),
                &format!("shot-{index}.png"),
                Duration::from_secs(3600 + 25 - index),
            )
        })
        .collect();
    let (path, _) = capture(&dir, &png_bytes(5, 5)).unwrap();
    let survivors: Vec<_> = paths.iter().filter(|path| path.exists()).cloned().collect();
    assert_eq!(survivors, paths[6..]);
    assert!(path.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 20);
}

#[test]
fn the_post_capture_sweep_never_deletes_the_capture_it_just_wrote() {
    // 20 retained files with future mtimes: without the keep guard the
    // fresh capture sorts last and would be swept away.
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    std::fs::create_dir_all(dir.path()).unwrap();
    let future = SystemTime::now() + Duration::from_secs(3600);
    for index in 0..20 {
        let path = dir.path().join(format!("future-{index}.png"));
        std::fs::write(&path, b"png").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(future)
            .unwrap();
    }
    let (path, _) = capture(&dir, &png_bytes(5, 5)).unwrap();
    assert!(path.exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 20);
}

#[test]
fn a_sweep_failure_never_breaks_the_capture() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    let undeletable = dir.path().join("stale-dir");
    std::fs::create_dir_all(&undeletable).unwrap();
    std::fs::File::open(&undeletable)
        .unwrap()
        .set_modified(SystemTime::now() - STALE)
        .unwrap();
    let (path, _) = capture(&dir, &png_bytes(5, 5)).unwrap();
    assert!(path.extension().is_some_and(|extension| extension == "png"));
    assert!(undeletable.exists());
}

#[test]
fn the_sweep_unlinks_a_planted_symlink_not_its_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    let stale = write_shot(dir.path(), "stale.png", STALE);
    let target = tmp.path().join("attacker-target.png");
    std::fs::write(&target, b"target data").unwrap();
    let link = dir.path().join("planted.png");
    symlink(&target, &link).unwrap();
    let old = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .saturating_sub(STALE);
    let stamp = rustix::fs::Timespec {
        tv_sec: i64::try_from(old.as_secs()).unwrap(),
        tv_nsec: 0,
    };
    // Age the link itself, not its target.
    rustix::fs::utimensat(
        rustix::fs::CWD,
        &link,
        &rustix::fs::Timestamps {
            last_access: stamp,
            last_modification: stamp,
        },
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
    capture(&dir, &png_bytes(5, 5)).unwrap();
    assert!(!stale.exists());
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the planted link is gone"
    );
    assert!(target.exists(), "the link's target was never touched");
}

#[test]
fn an_unusable_screenshot_dir_is_a_transport_error() {
    let tmp = tempfile::tempdir().unwrap();
    let blocked = tmp.path().join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    let error = CaptureDir::new(blocked, tmp.path().join("home"))
        .open()
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(
        error.message.contains("screenshot directory unavailable"),
        "{}",
        error.message
    );
}

#[test]
fn a_symlinked_capture_dir_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("elsewhere");
    std::fs::create_dir(&real).unwrap();
    let link = tmp.path().join("linked-shots");
    symlink(&real, &link).unwrap();
    let error = CaptureDir::new(link, tmp.path().join("home"))
        .open()
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(error.message.contains("symlink"));
}

#[test]
fn a_symlinked_parent_component_below_home_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("fake-home");
    let real = home.join("real-parent");
    std::fs::create_dir_all(&real).unwrap();
    let linked_parent = home.join(".prime").join("agent");
    std::fs::create_dir_all(linked_parent.parent().unwrap()).unwrap();
    symlink(&real, &linked_parent).unwrap();
    let shots = linked_parent.join("tmp").join("shots");
    let error = CaptureDir::new(shots.clone(), home).open().err().unwrap();
    assert!(error.message.contains("symlink"), "{}", error.message);
    assert!(!shots.exists());
}

#[test]
fn a_symlink_planted_after_the_guard_is_refused_by_the_open_chain() {
    // The guard passes (the link lands after it), and the no-follow chain
    // still refuses: validation and use cannot be separated.
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("fake-home");
    let real = home.join("real-parent");
    std::fs::create_dir_all(&real).unwrap();
    let shots = home.join(".prime").join("agent").join("tmp").join("shots");
    let dir = CaptureDir::new(shots.clone(), home.clone());
    dir.refuse_symlinked().unwrap();
    symlink(&real, home.join(".prime")).unwrap();
    let error = dir.open_chain().err().unwrap();
    assert_eq!(error.code, ErrorCode::TransportError);
    assert!(error.message.contains("symlink"), "{}", error.message);
    assert!(!shots.exists());
}

#[test]
fn missing_components_below_home_are_created_private() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("plain-home");
    std::fs::create_dir_all(&home).unwrap();
    let shots = home.join(".prime").join("agent").join("tmp").join("shots");
    let dir = CaptureDir::new(shots.clone(), home);
    capture(&dir, &png_bytes(5, 5)).unwrap();
    assert_eq!(mode(&shots), 0o700);
}

#[test]
fn system_symlinks_outside_home_are_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    symlink(&real, tmp.path().join("via")).unwrap();
    // A symlinked ancestor outside the home (like macOS /var -> /private/var).
    let dir = CaptureDir::new(
        tmp.path().join("via").join("shots"),
        tmp.path().join("home"),
    );
    capture(&dir, &png_bytes(5, 5)).unwrap();
}

#[test]
fn non_regular_and_symlinked_targets_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path()).with_fixed_name("fixed.png");
    std::fs::create_dir_all(dir.path().join("fixed.png")).unwrap();
    let error = dir.open().unwrap().new_target().err().unwrap();
    assert!(
        error.message.contains("not a regular file"),
        "{}",
        error.message
    );
    std::fs::remove_dir(dir.path().join("fixed.png")).unwrap();
    symlink(tmp.path(), dir.path().join("fixed.png")).unwrap();
    let error = dir.open().unwrap().new_target().err().unwrap();
    assert!(
        error.message.contains("not a regular file"),
        "{}",
        error.message
    );
}

#[test]
fn a_planted_fifo_never_blocks_the_read_back() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path()).with_fixed_name("fifo.png");
    let open = dir.open().unwrap();
    // The FIFO lands after the target check, before the read-back.
    let (name, path) = open.new_target().unwrap();
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(made.success());
    let started = std::time::Instant::now();
    let error = open.png_dimensions(&name).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(error.message.contains("regular PNG"), "{}", error.message);
}

#[test]
fn invalid_missing_and_empty_pngs_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = outside_home(tmp.path());
    let error = capture(&dir, b"not a png at all").unwrap_err();
    assert!(error.message.contains("valid PNG"), "{}", error.message);
    let error = capture(&dir, &png_bytes(0, 0)).unwrap_err();
    assert!(error.message.contains("empty PNG"), "{}", error.message);
    let open = dir.open().unwrap();
    let error = open.png_dimensions("never-written.png").unwrap_err();
    assert!(error.message.contains("readable PNG"), "{}", error.message);
}
