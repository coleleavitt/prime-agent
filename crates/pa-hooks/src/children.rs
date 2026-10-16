//! `SubagentStart`/`SubagentStop` for the kernel's RLM child agents: a
//! child starts when an `rlm.run` reply hands back its spawn handle, and
//! stops the first time the session learns it settled, from whichever comes
//! first: an `rlm.collect` result marked settled, an `rlm.list_subagents`
//! row no longer running, or the parent's terminal-notice row (a child that
//! ended without replying, failed, or was cancelled). Each child stops once.

use std::sync::{Mutex, PoisonError};

use serde_json::Value;

/// The most children tracked per session; the oldest is forgotten first
/// (a later settle then fires as an unknown child's).
const MAX_TRACKED_CHILDREN: usize = 256;

/// One child agent, as the hooks present it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    /// `agent_id`: the child's RLM id.
    pub id: String,
    /// `agent_type`: the child's session name (children have names, not
    /// roles).
    pub name: String,
    /// The child's session directory, when the host reported it.
    pub session_dir: Option<String>,
}

/// A child that settled, with its last words when known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledChild {
    pub child: Child,
    /// The child's final answer, or its error.
    pub last_message: Option<String>,
}

#[derive(Debug, Clone)]
struct Tracked {
    child: Child,
    stopped: bool,
}

/// One session's child agents.
#[derive(Debug, Default)]
pub struct Children {
    known: Mutex<Vec<Tracked>>,
}

fn string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl Children {
    /// The children still running, oldest first.
    #[must_use]
    pub fn running(&self) -> Vec<Child> {
        self.known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|tracked| !tracked.stopped)
            .map(|tracked| tracked.child.clone())
            .collect()
    }

    /// Read an `rlm.run` reply (the spawn handle): `Some` for the child it
    /// started.
    pub fn started(&self, reply: &Value) -> Option<Child> {
        let child = Child {
            id: string(reply, "rlm_child_id")?,
            name: string(reply, "name").unwrap_or_default(),
            session_dir: string(reply, "session_dir"),
        };
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        if known.iter().any(|tracked| tracked.child.id == child.id) {
            return None;
        }
        if known.len() >= MAX_TRACKED_CHILDREN {
            known.remove(0);
        }
        known.push(Tracked {
            child: child.clone(),
            stopped: false,
        });
        Some(child)
    }

    /// Mark child `id` stopped: `Some` the first time (an unknown child is
    /// tracked as stopped), `None` once it already stopped.
    fn settle(
        &self,
        id: String,
        name: Option<String>,
        session_dir: Option<String>,
        last_message: Option<String>,
    ) -> Option<SettledChild> {
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        let child = match known.iter_mut().find(|tracked| tracked.child.id == id) {
            Some(tracked) if tracked.stopped => return None,
            Some(tracked) => {
                tracked.stopped = true;
                if tracked.child.session_dir.is_none() {
                    tracked.child.session_dir = session_dir;
                }
                tracked.child.clone()
            }
            None => {
                let child = Child {
                    id,
                    name: name.unwrap_or_default(),
                    session_dir,
                };
                if known.len() >= MAX_TRACKED_CHILDREN {
                    known.remove(0);
                }
                known.push(Tracked {
                    child: child.clone(),
                    stopped: true,
                });
                child
            }
        };
        Some(SettledChild {
            child,
            last_message,
        })
    }

    /// The children a host request's reply reports settled for the first
    /// time: `rlm.collect` results marked `settled`, `rlm.list_subagents`
    /// rows no longer `running`.
    pub fn settled_in_reply(&self, request_type: &str, reply: &Value) -> Vec<SettledChild> {
        let (rows, key) = match request_type {
            "rlm.collect" => (reply.get("results"), "results"),
            "rlm.list_subagents" => (reply.get("subagents"), "subagents"),
            _ => return Vec::new(),
        };
        let Some(rows) = rows.and_then(Value::as_array) else {
            return Vec::new();
        };
        rows.iter()
            .filter(|row| {
                if key == "results" {
                    row.get("settled").and_then(Value::as_bool) == Some(true)
                } else {
                    row.get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|status| status != "running")
                }
            })
            .filter_map(|row| {
                self.settle(
                    string(row, "rlm_child_id")?,
                    string(row, "session_name"),
                    string(row, "session_dir"),
                    string(row, "answer_text")
                        .or_else(|| string(row, "answer_preview"))
                        .or_else(|| string(row, "error")),
                )
            })
            .collect()
    }

    /// The child a terminal-notice row (`rlm_child_terminal_notice`,
    /// `rlm_child_failure`, by its `details`) reports settled for the
    /// first time.
    pub fn settled_by_notice(&self, details: &Value) -> Option<SettledChild> {
        self.settle(
            string(details, "childId")?,
            string(details, "sessionName"),
            None,
            string(details, "lastAssistantTextPreview").or_else(|| string(details, "error")),
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn child(id: &str, name: &str, dir: Option<&str>) -> Child {
        Child {
            id: id.to_string(),
            name: name.to_string(),
            session_dir: dir.map(str::to_string),
        }
    }

    #[test]
    fn a_child_starts_once_and_stops_once_from_whichever_source_is_first() {
        let children = Children::default();
        let spawned = json!({ "rlm_child_id": "sub-1", "name": "reviewer", "session_dir": "/s/sub-1", "model": "m" });
        assert_eq!(
            children.started(&spawned),
            Some(child("sub-1", "reviewer", Some("/s/sub-1")))
        );
        assert_eq!(children.started(&spawned), None);
        children.started(&json!({ "rlm_child_id": "sub-2", "name": "worker" }));
        assert_eq!(children.running().len(), 2);
        // A running result settles nothing; a settled one settles once.
        let running = json!({ "results": [{ "rlm_child_id": "sub-1", "status": "running", "settled": false }] });
        assert!(
            children
                .settled_in_reply("rlm.collect", &running)
                .is_empty()
        );
        let done = json!({ "results": [{ "rlm_child_id": "sub-1", "status": "done", "settled": true, "answer_text": "LGTM" }] });
        assert_eq!(
            children.settled_in_reply("rlm.collect", &done),
            vec![SettledChild {
                child: child("sub-1", "reviewer", Some("/s/sub-1")),
                last_message: Some("LGTM".to_string()),
            }]
        );
        assert!(children.settled_in_reply("rlm.collect", &done).is_empty());
        // The roster and the notice row settle the other child once.
        let roster = json!({ "subagents": [
            { "rlm_child_id": "sub-1", "session_name": "reviewer", "status": "completed" },
            { "rlm_child_id": "sub-2", "session_name": "worker", "status": "error", "error": "boom" },
        ] });
        assert_eq!(
            children.settled_in_reply("rlm.list_subagents", &roster),
            vec![SettledChild {
                child: child("sub-2", "worker", None),
                last_message: Some("boom".to_string()),
            }]
        );
        assert_eq!(
            children.settled_by_notice(&json!({ "childId": "sub-2", "error": "boom" })),
            None
        );
        assert!(children.running().is_empty());
        // A child this session never saw start settles as itself.
        assert_eq!(
            children.settled_by_notice(&json!({
                "kind": "completed_without_reply", "childId": "sub-9",
                "sessionName": "lane", "lastAssistantTextPreview": "done"
            })),
            Some(SettledChild {
                child: child("sub-9", "lane", None),
                last_message: Some("done".to_string()),
            })
        );
        assert!(
            children
                .settled_in_reply("rlm.rename", &json!({ "results": [] }))
                .is_empty()
        );
    }
}
