//! Link the session to the pull request a command of its creates (Claude Code's
//! `linkSessionToPR` after a successful `gh pr create`): the kernel's `bash()` reaches the host
//! as `bash.run` (a short command's result rides its reply) or a later `bash.follow` for the same
//! job, so the gate remembers each running creation command by job id until its `finished` event
//! arrives. A creation that exits 0 with a pull or merge request URL in its output appends a
//! [`PR_LINK_CUSTOM_TYPE`] entry to the session, which the saved-session catalog folds into the
//! row the resume picker shows and `--from-pr` filters on. Review passes through to the inner
//! gate untouched.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;
use serde_json::Value;

use crate::kernel::shared::{HostRequestGate, HostRequestPolicy};
use crate::session::manager::SessionManager;
use crate::session::pr_link::{
    PR_LINK_CUSTOM_TYPE,
    SessionPrLink,
    creates_pull_request,
    find_pr_link,
};

/// The most running creation commands remembered; the oldest is forgotten first.
const MAX_TRACKED_JOBS: usize = 16;

/// The most of a spilled result read back to find the URL.
const MAX_SPILLED_OUTPUT: u64 = 64 * 1024;

type GateFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The pull-request link gate in front of `inner`.
pub(crate) fn pr_link_gate(
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    inner: Option<HostRequestGate>,
) -> HostRequestGate {
    Arc::new(PrLinkGate {
        session,
        inner,
        tracker: Arc::new(PrLinkTracker::default()),
    })
}

struct PrLinkGate {
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    inner: Option<HostRequestGate>,
    tracker: Arc<PrLinkTracker>,
}

impl HostRequestPolicy for PrLinkGate {
    fn review(&self, request_type: &str, data: &Value) -> GateFuture<Option<String>> {
        match &self.inner {
            Some(inner) => inner.review(request_type, data),
            None => Box::pin(async { None }),
        }
    }

    fn after(
        &self,
        request_type: &str,
        data: &Value,
        outcome: Result<&Value, &str>,
    ) -> GateFuture<()> {
        let inner = self
            .inner
            .as_ref()
            .map(|inner| inner.after(request_type, data, outcome));
        let link = outcome
            .ok()
            .and_then(|reply| self.tracker.observe(request_type, data, reply));
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            if let Some(inner) = inner {
                inner.await;
            }
            let Some(link) = link else {
                return;
            };
            let Ok(data) = serde_json::to_value(&link) else {
                return;
            };
            if let Err(error) = session
                .lock()
                .await
                .append_custom_entry(PR_LINK_CUSTOM_TYPE, Some(data))
            {
                tracing::warn!(%error, "the pull-request link was not recorded");
            }
        })
    }
}

/// The running creation commands by job id, and the last link recorded (a repeated report of
/// the same pull request records nothing).
#[derive(Default)]
struct PrLinkTracker {
    running: Mutex<Vec<String>>,
    last: Mutex<Option<SessionPrLink>>,
}

impl PrLinkTracker {
    /// One `bash.*` reply: the link to record, when it finishes a pull-request creation.
    fn observe(&self, request_type: &str, data: &Value, reply: &Value) -> Option<SessionPrLink> {
        if reply.get("status").and_then(Value::as_str) != Some("ok") {
            return None;
        }
        match request_type {
            "bash.run" => {
                let command = data.get("command").and_then(Value::as_str)?;
                if !creates_pull_request(command) {
                    return None;
                }
                if finished_event(reply).is_none() {
                    let id = reply.get("job")?.get("id")?.as_str()?;
                    let mut running = self.running.lock_or_recover();
                    if running.len() >= MAX_TRACKED_JOBS {
                        running.remove(0);
                    }
                    running.push(id.to_string());
                    return None;
                }
            }
            "bash.follow" => {
                let id = data.get("id").and_then(Value::as_str)?;
                finished_event(reply)?;
                let mut running = self.running.lock_or_recover();
                let index = running.iter().position(|job| job == id)?;
                running.remove(index);
            }
            _ => return None,
        }
        let event = finished_event(reply)?;
        if event.get("exitCode").and_then(Value::as_i64) != Some(0) {
            return None;
        }
        let link = find_pr_link(&output_of(event))?;
        let mut last = self.last.lock_or_recover();
        if last.as_ref() == Some(&link) {
            return None;
        }
        *last = Some(link.clone());
        Some(link)
    }
}

/// The `finished` event of a `bash.*` reply, when it carries one.
fn finished_event(reply: &Value) -> Option<&Value> {
    reply
        .get("events")
        .and_then(Value::as_array)?
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some("finished"))
}

/// The command's output: inline, or read back (capped) from the file a large result spilled to.
fn output_of(event: &Value) -> String {
    if let Some(output) = event.get("output").and_then(Value::as_str) {
        return output.to_string();
    }
    let Some(path) = event.get("outputFile").and_then(Value::as_str) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| {
        use std::io::Read;
        file.take(MAX_SPILLED_OUTPUT).read_to_end(&mut bytes)
    });
    match read {
        Ok(_) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CREATED: &str = "Creating pull request for topic into main in acme/app\n\nhttps://github.com/acme/app/pull/42\n";

    fn finished(output: &str, exit: i32) -> Value {
        json!([{ "type": "finished", "exitCode": exit, "duration": 0.1, "fields": {}, "output": output }])
    }

    fn reply(job: &str, events: &Value) -> Value {
        json!({ "status": "ok", "job": { "id": job, "pid": 1 }, "events": events, "cursor": 0, "done": false })
    }

    fn link(number: u64) -> SessionPrLink {
        SessionPrLink {
            number,
            url: format!("https://github.com/acme/app/pull/{number}"),
            repository: "acme/app".to_string(),
        }
    }

    #[test]
    fn a_creation_that_finishes_in_its_run_reply_links_once() {
        let tracker = PrLinkTracker::default();
        let run = json!({ "type": "bash.run", "command": "gh pr create --fill" });
        let done = reply("j1", &finished(CREATED, 0));
        assert_eq!(tracker.observe("bash.run", &run, &done), Some(link(42)));
        // The same pull request reported again records nothing.
        assert_eq!(tracker.observe("bash.run", &run, &done), None);
    }

    #[test]
    fn a_long_creation_links_when_its_follow_finishes() {
        let tracker = PrLinkTracker::default();
        let run = json!({ "command": "git push && gh pr create --fill" });
        assert_eq!(
            tracker.observe("bash.run", &run, &reply("j1", &json!([]))),
            None
        );
        let follow = json!({ "type": "bash.follow", "id": "j1" });
        assert_eq!(
            tracker.observe("bash.follow", &follow, &reply("j1", &json!([]))),
            None
        );
        assert_eq!(
            tracker.observe("bash.follow", &follow, &reply("j1", &finished(CREATED, 0))),
            Some(link(42))
        );
    }

    #[test]
    fn failures_and_other_commands_link_nothing() {
        let tracker = PrLinkTracker::default();
        let create = json!({ "command": "gh pr create --fill" });
        assert_eq!(
            tracker.observe("bash.run", &create, &reply("j1", &finished(CREATED, 1))),
            None
        );
        let view = json!({ "command": "gh pr view 42" });
        assert_eq!(
            tracker.observe("bash.run", &view, &reply("j2", &finished(CREATED, 0))),
            None
        );
        let refused = json!({ "status": "error", "error": "nope" });
        assert_eq!(tracker.observe("bash.run", &create, &refused), None);
        // A follow of an untracked job is someone else's.
        let follow = json!({ "id": "j9" });
        assert_eq!(
            tracker.observe("bash.follow", &follow, &reply("j9", &finished(CREATED, 0))),
            None
        );
    }

    #[tokio::test]
    async fn the_gate_records_the_link_in_the_session() {
        let session = Arc::new(tokio::sync::Mutex::new(SessionManager::in_memory(
            std::path::Path::new("/tmp"),
        )));
        let gate = pr_link_gate(Arc::clone(&session), None);
        assert_eq!(
            gate.review("bash.run", &json!({ "command": "gh pr create" }))
                .await,
            None
        );
        let run = json!({ "type": "bash.run", "command": "gh pr create --fill" });
        gate.after("bash.run", &run, Ok(&reply("j1", &finished(CREATED, 0))))
            .await;
        let entries = serde_json::to_value(session.lock().await.get_entries()).unwrap();
        let custom: Vec<&Value> = entries
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| {
                entry.get("customType").and_then(Value::as_str) == Some(PR_LINK_CUSTOM_TYPE)
            })
            .collect();
        assert_eq!(custom.len(), 1);
        assert_eq!(custom[0]["data"], serde_json::to_value(link(42)).unwrap());
    }
}
