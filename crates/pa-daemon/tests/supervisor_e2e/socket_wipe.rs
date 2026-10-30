//! A wiped socket directory must not end the daemon: it takes its lock directory and its
//! socket path again, and clients connect to the same path without a restart.

use super::*;

#[test]
fn a_wiped_socket_directory_is_rebuilt_while_the_supervisor_keeps_running() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let run = dir.path().join("run");
    let socket = run.join("daemon.sock");
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(&run).expect("socket dir");
    let mut daemon = spawn_daemon(&socket, &agent_dir);
    let (first, hello) = Client::connect(&socket);
    assert_eq!(hello["type"], "daemon_hello");

    std::fs::remove_dir_all(&run).expect("wipe the socket directory");

    let (_second, hello) = Client::connect(&socket);
    assert_eq!(
        hello["type"], "daemon_hello",
        "a client reaches the daemon at the same path"
    );
    assert!(
        daemon.child.try_wait().expect("poll supervisor").is_none(),
        "the supervisor survived the wipe"
    );
    let lock = run.join("daemon.sock.lock");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !lock.is_dir() {
        assert!(
            Instant::now() < deadline,
            "the lock directory was not retaken"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(first);
}
