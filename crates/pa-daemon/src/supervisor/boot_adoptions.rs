//! The boot adoption pass's unsettled descriptors: a command addressed to one
//! of their sessions waits for its adoption instead of failing as unknown
//! while the pass still relaunches the session's worker.

use std::path::{Path, PathBuf};

use pa_types::sync::MutexExt;

use crate::descriptor::WorkerDescriptor;
use crate::registry::selector_matches;

/// One descriptor the pass has not settled: the identities a selector can
/// name its session by (the registry's resolve rules).
struct PendingAdoption {
    worker_id: String,
    file_stem: String,
}

/// The descriptors the boot adoption pass is still adopting or reviving. A
/// revived worker joins the registry only once its relaunch finished, so a
/// command for its session (a restarted parent probing its children) would
/// otherwise race the relaunch and read the session as gone.
#[derive(Default)]
pub(crate) struct BootAdoptions {
    pending: std::sync::Mutex<Vec<PendingAdoption>>,
    settled: tokio::sync::Notify,
}

impl BootAdoptions {
    /// The pass starts on these descriptors.
    pub(super) fn begin(&self, descriptors: &[(PathBuf, WorkerDescriptor)]) {
        let mut pending = self.pending.lock_or_recover();
        pending.extend(descriptors.iter().map(|(_, descriptor)| {
            PendingAdoption {
                worker_id: descriptor.worker_id.clone(),
                file_stem: descriptor
                    .session_file
                    .as_deref()
                    .and_then(|file| Path::new(file).file_stem())
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            }
        }));
    }

    /// The pass settled one descriptor (adopted, revived, or left down).
    pub(super) fn settle(&self, worker_id: &str) {
        self.pending
            .lock_or_recover()
            .retain(|adoption| adoption.worker_id != worker_id);
        self.settled.notify_waiters();
    }

    fn owns(&self, selector: &str) -> bool {
        self.pending.lock_or_recover().iter().any(|adoption| {
            selector_matches(&adoption.worker_id, selector)
                || selector_matches(&adoption.file_stem, selector)
        })
    }

    /// Wait until no unsettled descriptor names `selector`; a selector the
    /// pass does not own returns at once.
    pub(super) async fn wait_for(&self, selector: &str) {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            // Registered before the check: a settle between the check and
            // the await still wakes this waiter.
            settled.as_mut().enable();
            if !self.owns(selector) {
                return;
            }
            settled.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use pa_types::daemon::DaemonCommand;
    use serde_json::{Value, json};

    use super::super::{Supervisor, SupervisorOptions, subscribers};

    async fn get_state(supervisor: &Arc<Supervisor>, selector: &str) -> Value {
        let command: DaemonCommand =
            serde_json::from_value(json!({ "type": "get_state", "activeSessionId": selector }))
                .expect("command");
        let (queue_tx, _queue_rx) = tokio::sync::mpsc::channel(4);
        let attached = subscribers::ClientSubscriptions::new("conn".to_string(), queue_tx);
        let (lines, _) = supervisor
            .route_client_command(
                &command,
                "client",
                &attached,
                "c1".into(),
                "get_state".into(),
                None,
            )
            .await;
        lines[0].clone()
    }

    /// A command for a session the boot pass is still reviving waits for
    /// the pass to settle it (a restarted parent's child probe must not
    /// read a reviving child as gone); any other session answers at once.
    #[tokio::test]
    async fn a_command_for_a_reviving_session_waits_for_its_adoption() {
        let dir = tempfile::TempDir::new().unwrap();
        let supervisor = Arc::new(
            Supervisor::new(SupervisorOptions {
                tcp_port: None,
                tcp_bind_host: None,
                remote_agent_mesh: None,
                socket_path: dir.path().join("daemon.sock"),
                agent_dir: dir.path().join("agent"),
            })
            .expect("supervisor"),
        );
        let descriptor = serde_json::from_value(json!({
            "version": 2, "workerId": "w-child", "pid": 4242, "socketPath": "/tmp/none.sock",
            "recoveryJournalPath": "/tmp/none.jsonl", "supervisorSocketPath": "/tmp/none.sock",
            "authenticationToken": "token", "rootActiveSessionId": "w-child", "createdAt": "t",
            "updatedAt": "t", "lifecycle": "ready", "createCommand": {},
            "sessionFile": "/sessions/01child.jsonl", "consecutiveFailures": 0,
        }))
        .expect("descriptor");
        supervisor
            .boot_adoptions
            .begin(&[(dir.path().join("w-child.json"), descriptor)]);

        let unrelated = get_state(&supervisor, "01other").await;
        assert_eq!(unrelated["error"], "Unknown active session: 01other");

        let mut waiting = tokio::spawn({
            let supervisor = Arc::clone(&supervisor);
            async move { get_state(&supervisor, "01child").await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut waiting)
                .await
                .is_err(),
            "the command waits while the pass revives its session"
        );
        supervisor.boot_adoptions.settle("w-child");
        let answered = waiting.await.expect("the waiting command");
        assert_eq!(answered["error"], "Unknown active session: 01child");
    }
}
