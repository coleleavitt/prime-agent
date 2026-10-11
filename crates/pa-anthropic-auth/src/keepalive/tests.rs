//! Keep-alive passes against a temporary store and a loopback token
//! endpoint (called directly: no thread, no clock waits).

use std::sync::atomic::Ordering;

use anthropic::AccountStore;
use anthropic::token::Credential;
use chrono::{Duration, Utc};

use super::KeepAliveTick;
use crate::test_support::*;
use crate::{SharedStoreConfig, SharedStoreSource};

fn refresh_token_of(source: &SharedStoreSource, id: &str) -> Option<String> {
    AccountStore::load(source.store_path())
        .expect("the store")
        .get(id)
        .and_then(anthropic::Account::oauth)
        .map(|tokens| tokens.refresh.expose().to_string())
}

fn pass(source: &SharedStoreSource) -> KeepAliveTick {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(source.keepalive().tick(source.client(), Utc::now()))
}

#[test]
fn a_login_in_use_is_refreshed_ahead_of_its_expiry() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("ahead", Duration::minutes(10))], &token_url);
    pa_core::auth::ProviderCredentialSource::credential(source.as_ref()).expect("served");

    assert_eq!(
        pass(&source),
        KeepAliveTick {
            ahead_refreshed: 1,
            ..KeepAliveTick::default()
        }
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        refresh_token_of(&source, "ahead"),
        Some("sk-ant-ort01-rotated-rotated-rotated-00".to_string())
    );
}

#[test]
fn a_login_far_from_expiry_or_not_in_use_is_left_alone() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(
        vec![
            row("far", Duration::hours(2)),
            row("unused", Duration::minutes(10)),
        ],
        &token_url,
    );
    pa_core::auth::ProviderCredentialSource::credential(source.as_ref()).expect("served");

    assert_eq!(pass(&source), KeepAliveTick::default());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn an_idle_login_whose_refresh_token_nears_expiry_is_kept_alive() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let mut idle = row("idle", Duration::hours(-1));
    if let Credential::Oauth(tokens) = &mut idle.credential {
        tokens.refresh_expires_at = Some(Utc::now() + Duration::days(3));
    }
    let (_home, source) = source_over(vec![idle], &token_url);

    assert_eq!(
        pass(&source),
        KeepAliveTick {
            idle_refreshed: 1,
            ..KeepAliveTick::default()
        }
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn the_keepalive_thread_starts_with_the_first_served_credential() {
    let (_home, seeded) = source_over(
        vec![row("thread", Duration::hours(2))],
        "http://127.0.0.1:9",
    );
    let mut config = SharedStoreConfig::isolated(
        seeded.store_path().to_path_buf(),
        "http://127.0.0.1:9/v1/oauth/token",
        "http://127.0.0.1:9/api/oauth/profile",
    );
    config.background = true;
    let source = SharedStoreSource::new(config);

    // Construction and status reads start nothing.
    assert!(pa_core::auth::ProviderCredentialSource::status(&source).is_some());
    assert!(!source.keepalive_started());

    pa_core::auth::ProviderCredentialSource::credential(&source).expect("served");
    assert!(source.keepalive_started());
}

/// A keep-alive whose version lookup reaches `url`.
fn version_reader(url: &str) -> super::KeepAlive {
    let mut config = SharedStoreConfig::isolated(
        std::path::PathBuf::from("/nonexistent/accounts.json"),
        "http://127.0.0.1:9/v1/oauth/token",
        "http://127.0.0.1:9/api/oauth/profile",
    );
    config.version_url = Some(url.to_string());
    super::KeepAlive::new(config, std::sync::Arc::default())
}

fn read_version(keepalive: &super::KeepAlive) -> String {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
        .block_on(keepalive.refresh_version(Utc::now()));
    keepalive.claude_code_version()
}

#[test]
fn the_live_claude_code_version_is_read_and_cached() {
    let (url, hits) = token_endpoint(
        200,
        r#"{"name":"@anthropic-ai/claude-code","version":"2.1.400"}"#,
    );
    let keepalive = version_reader(&url);
    assert_eq!(keepalive.claude_code_version(), "2.1.280");

    assert_eq!(read_version(&keepalive), "2.1.400");
    // Cached for the hour: no second lookup.
    assert_eq!(read_version(&keepalive), "2.1.400");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn an_older_or_unreadable_version_keeps_the_floor() {
    let (older, _) = token_endpoint(200, r#"{"version":"2.1.100"}"#);
    assert_eq!(read_version(&version_reader(&older)), "2.1.280");
    let (failing, _) = token_endpoint(500, "{}");
    assert_eq!(read_version(&version_reader(&failing)), "2.1.280");
}

/// Hold the store lock on another thread until the returned sender is
/// dropped or sent to (a peer's store write stalled on a loaded disk).
fn hold_store_lock(
    source: &SharedStoreSource,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (held, holding) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let path = source.store_path().to_path_buf();
    let holder = std::thread::spawn(move || {
        AccountStore::mutate(&path, |_| {
            held.send(()).expect("report the lock held");
            let _ = released.recv();
            Ok(())
        })
        .expect("the peer's write");
    });
    holding.recv().expect("the peer holds the lock");
    (release, holder)
}

#[test]
fn a_pass_with_nothing_due_does_not_queue_behind_the_store_lock() {
    let (token_url, hits) = token_endpoint(200, ROTATED);
    let (_home, source) = source_over(vec![row("not-due", Duration::hours(2))], &token_url);
    let (release, holder) = hold_store_lock(&source);
    let warnings = WarningLog::default();

    let tick = warnings.capture(|| pass(&source));
    release.send(()).expect("release the peer");
    holder.join().expect("the peer");

    assert_eq!(tick, KeepAliveTick::default());
    assert_eq!(warnings.messages(), Vec::<String>::new());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn a_refresh_in_flight_does_not_hold_up_the_pass() {
    // The token endpoint stalls the refresh until the test lets it answer.
    let (arrived, arriving) = std::sync::mpsc::channel();
    let (answer, answering) = std::sync::mpsc::channel::<()>();
    let gate = std::sync::Mutex::new((arrived, answering));
    let (token_url, hits) = token_endpoint_then(200, ROTATED, move || {
        let gate = pa_types::sync::MutexExt::lock_or_recover(&gate);
        let _ = gate.0.send(());
        let _ = gate.1.recv();
    });
    let (_home, source) = source_over(vec![row("in-flight", Duration::hours(-1))], &token_url);
    let requester = {
        let source = std::sync::Arc::clone(&source);
        std::thread::spawn(move || {
            pa_core::auth::ProviderCredentialSource::credential(source.as_ref())
                .map(|credential| credential.api_key)
        })
    };
    arriving.recv().expect("the refresh reached the endpoint");
    let warnings = WarningLog::default();

    // The row's refresh claim is held while its token is in flight: the
    // pass leaves it alone, and the store lock is free meanwhile.
    let tick = warnings.capture(|| pass(&source));
    answer.send(()).expect("let the endpoint answer");

    assert_eq!(tick, KeepAliveTick::default());
    assert_eq!(warnings.messages(), Vec::<String>::new());
    assert_eq!(
        requester.join().expect("the request"),
        Ok(ROTATED_ACCESS.to_string())
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

/// Set in the process [`store_lock_holder`] runs in: the store's path.
const HOLDER_STORE_ENV: &str = "PA_ANTHROPIC_AUTH_KEEPALIVE_HOLDER_STORE";
/// What [`store_lock_holder`] prints once it holds the lock (the test
/// harness may print its own text on the same line).
const HOLDER_LOCKED: &str = "store-lock-held";

/// A process that takes the store lock, says so on stdout
/// ([`HOLDER_LOCKED`]) and waits inside it until it is killed. Without its setup (a plain `--ignored` run) it does
/// nothing.
#[test]
#[ignore = "a child process of a_holder_killed_inside_the_store_lock_does_not_fail_the_pass"]
fn store_lock_holder() {
    let Ok(path) = std::env::var(HOLDER_STORE_ENV) else {
        return;
    };
    AccountStore::mutate(std::path::Path::new(&path), |_| {
        println!("{HOLDER_LOCKED}");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        Ok(())
    })
    .expect("the store lock");
}

#[test]
fn a_holder_killed_inside_the_store_lock_does_not_fail_the_pass() {
    use std::io::BufRead as _;

    let (token_url, hits) = token_endpoint(200, ROTATED);
    let mut idle = row("orphaned-lock", Duration::hours(-1));
    if let Credential::Oauth(tokens) = &mut idle.credential {
        tokens.refresh_expires_at = Some(Utc::now() + Duration::days(3));
    }
    let (_home, source) = source_over(vec![idle], &token_url);
    // A host process killed while one of its threads writes the store.
    let mut holder = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            "keepalive::tests::store_lock_holder",
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HOLDER_STORE_ENV, source.store_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("start the holder");
    let stdout = holder.stdout.take().expect("the holder's stdout");
    let locked = std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .any(|line| line.contains(HOLDER_LOCKED));
    assert!(locked, "the holder took the store lock");
    holder.kill().expect("kill the holder");
    holder.wait().expect("the holder is gone");
    let warnings = WarningLog::default();

    let tick = warnings.capture(|| pass(&source));

    assert_eq!(
        tick,
        KeepAliveTick {
            idle_refreshed: 1,
            ..KeepAliveTick::default()
        }
    );
    assert_eq!(warnings.messages(), Vec::<String>::new());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
