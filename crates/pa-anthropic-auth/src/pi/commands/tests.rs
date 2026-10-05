//! The settings commands against the plugin's own runs
//! (`tests/fixtures/golden/pi_requests.json` → `commands`): the text each
//! prints and the settings file after it, byte for byte.

use super::*;
use crate::pi::convert::tests::golden;

#[test]
fn every_command_prints_and_writes_what_pi_does() {
    for sequence in golden()["commands"].as_array().expect("command sequences") {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let path = directory.path().join("anthropic-auth.json");
        if let Some(initial) = sequence["initial"].as_str() {
            std::fs::write(&path, initial).expect("the initial settings");
        }
        let settings = PluginSettings::new(path.clone());
        for step in sequence["steps"].as_array().expect("steps") {
            let args = step["args"].as_str().expect("arguments");
            let text = match step["command"].as_str().expect("a command") {
                FAST_COMMAND => run_fast(&settings, args),
                CACHE_COMMAND => run_cache(&settings, args),
                other => panic!("unexpected command {other}"),
            }
            .expect("the command runs");
            let label = format!("{} /{} {args:?}", sequence["name"], step["command"]);
            assert_eq!(text, step["text"].as_str().expect("the text"), "{label}");
            assert_eq!(
                std::fs::read_to_string(&path).ok(),
                step["file"].as_str().map(str::to_string),
                "{label}"
            );
        }
        // The lock is released.
        assert!(!directory
            .path()
            .join("anthropic-auth.json.config-write.lock")
            .exists());
    }
}

#[test]
fn a_command_waits_for_a_live_lock() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("anthropic-auth.json");
    let lock = directory
        .path()
        .join("anthropic-auth.json.config-write.lock");
    let expires = chrono::Utc::now().timestamp_millis() + 60_000;
    std::fs::write(
        &lock,
        format!("{{\"ownerId\":\"live\",\"expiresAt\":{expires}}}\n"),
    )
    .expect("a live lock");
    let (sender, receiver) = std::sync::mpsc::channel();
    let writer = {
        let path = path.clone();
        std::thread::spawn(move || {
            let settings = PluginSettings::new(path);
            let result = run_fast(&settings, "on");
            sender.send(()).expect("report");
            result
        })
    };
    // The writer is still waiting while the lock is held.
    assert!(receiver
        .recv_timeout(std::time::Duration::from_millis(200))
        .is_err());
    assert!(!path.exists());
    std::fs::remove_file(&lock).expect("release the lock");
    writer
        .join()
        .expect("the writer")
        .expect("the command runs");
    assert!(request_settings(&PluginSettings::new(path).read()).fast_mode);
}

#[test]
fn a_command_takes_over_an_expired_lock() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("anthropic-auth.json");
    let lock = directory
        .path()
        .join("anthropic-auth.json.config-write.lock");
    std::fs::write(&lock, "{\"ownerId\":\"dead\",\"expiresAt\":1}\n").expect("an expired lock");
    let settings = PluginSettings::new(path);

    run_fast(&settings, "on").expect("an expired lock is taken over");

    assert!(request_settings(&settings.read()).fast_mode);
    assert!(!lock.exists());
}

#[test]
fn a_corrupt_settings_file_is_refused_not_overwritten() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = directory.path().join("anthropic-auth.json");
    std::fs::write(&path, "{not json").expect("a corrupt file");
    let settings = PluginSettings::new(path.clone());

    let error = run_fast(&settings, "on").expect_err("a corrupt file is refused");

    assert!(matches!(error, SettingsError::Corrupt { .. }), "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).ok().as_deref(),
        Some("{not json")
    );
}
