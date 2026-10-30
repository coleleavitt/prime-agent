#![cfg(unix)]
//! The worker's serve loop keeps its socket path reachable.
use std::time::Duration;

use super::*;

/// A /tmp wipe unlinks the worker's socket file, and its directory, while the process
/// keeps listening on the unlinked inode; the supervisor's adoption then fails on a
/// missing path forever. The worker binds the path again.
#[tokio::test]
async fn a_removed_socket_file_is_bound_again() {
    let dir = crate::test_support::TestDir::new("pa-worker-rebind-");
    let run = dir.join("run");
    std::fs::create_dir_all(&run).expect("socket dir");
    let socket = run.join("worker.sock");
    let worker = Arc::new(Worker::new(
        WorkerConfig {
            socket_path: socket.clone(),
            supervisor_socket_path: std::path::PathBuf::new(),
            token: "token".to_string(),
            worker_instance_id: String::new(),
            active_session_id: "rebind-session".to_string(),
            agent_dir: dir.join("agent"),
            recovery_journal_path: dir.join("recovery.jsonl"),
            telemetry_disabled: None,
            script: Some(json!({ "responses": [] })),
        },
        None,
    ));
    let serving = tokio::spawn(Arc::clone(&worker).serve());
    let ready = async {
        while !crate::socket::can_connect(&socket, Duration::from_millis(200)).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), ready)
        .await
        .expect("the worker binds its socket");
    let first = crate::socket::socket_identity(&socket);

    std::fs::remove_dir_all(&run).expect("remove the socket directory");
    let rebound = async {
        while !crate::socket::can_connect(&socket, Duration::from_millis(200)).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), rebound)
        .await
        .expect("the worker binds the removed path again");
    assert_ne!(crate::socket::socket_identity(&socket), first);
    assert_eq!(
        *worker.bound_socket_identity.lock().unwrap(),
        crate::socket::socket_identity(&socket)
    );
    serving.abort();
}
