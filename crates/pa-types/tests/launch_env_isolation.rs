//! A libtest harness's launch env never forwards the real state its runner
//! pointed at: a `cargo test` started from a prime-agent session inherits
//! the session's `PRIME_AGENT_SESSION_DIR`, and a test client's `create`
//! handed it to the worker, which refused to start.

use pa_types::daemon::launch_env::collect_process_launch_env;
use pa_types::platform::test_isolation::PROTECTED_HOME_ENV;

#[test]
fn the_launch_env_drops_an_inherited_redirect_into_the_real_state() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join(".prime/agent/sessions");
    // Before anything in this process isolated its environment, as a
    // session's `cargo test` starts.
    pa_types::env::set_var(PROTECTED_HOME_ENV, home.path());
    pa_types::env::set_var("PRIME_AGENT_SESSION_DIR", &sessions);

    let env = collect_process_launch_env();

    assert_eq!(env.get("PRIME_AGENT_SESSION_DIR"), None);
}
