//! The worker's side of a kernel-made worktree move
//! ([`crate::worktree_host`]): the session branch records it (a
//! `session_cwd_state` entry, the one `/cwd` writes, so a resume restarts in
//! the moved-to directory, and a `worktree_session_state` entry for the
//! exit), the session and engine follow, and attached clients get
//! `cwd_changed`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use pa_types::sync::MutexExt;
use serde_json::json;

use super::session_cwd::SESSION_CWD_STATE_CUSTOM_TYPE;
use super::{EventPump, SessionCore, emit_worker_event_with};
use crate::engine::SessionEngine as _;
use crate::worktree_host::{
    WORKTREE_SESSION_STATE_CUSTOM_TYPE,
    WorktreeSession,
    WorktreeSessionState,
    WorktreeSessionView,
    latest_state,
};

pub(crate) struct WorkerWorktreeSeam {
    core: Arc<Mutex<SessionCore>>,
    events: Arc<EventPump>,
    engine: Weak<crate::agent_engine::AgentSessionEngine>,
}

impl WorkerWorktreeSeam {
    pub(crate) fn new(
        core: Arc<Mutex<SessionCore>>,
        events: Arc<EventPump>,
        engine: Weak<crate::agent_engine::AgentSessionEngine>,
    ) -> Self {
        Self {
            core,
            events,
            engine,
        }
    }
}

impl WorktreeSession for WorkerWorktreeSeam {
    fn view(&self) -> WorktreeSessionView {
        let core = self.core.lock_or_recover();
        let state = core.store.as_ref().and_then(|store| {
            latest_state(
                store
                    .branch()
                    .into_iter()
                    .filter(|entry| entry.type_ == "custom")
                    .map(|entry| &entry.fields)
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        });
        WorktreeSessionView {
            cwd: core.cwd.clone(),
            session_id: core.active_session_id.clone(),
            state,
        }
    }

    fn moved(&self, cwd: &str, state: &WorktreeSessionState) -> anyhow::Result<()> {
        {
            let mut core = self.core.lock_or_recover();
            if let Some(store) = core.store.as_mut() {
                store.persist_entry(
                    "custom",
                    json!({
                        "customType": SESSION_CWD_STATE_CUSTOM_TYPE,
                        "data": { "cwd": cwd },
                    }),
                )?;
                store.persist_entry(
                    "custom",
                    json!({
                        "customType": WORKTREE_SESSION_STATE_CUSTOM_TYPE,
                        "data": state,
                    }),
                )?;
            }
            core.cwd = cwd.to_string();
        }
        if let Some(engine) = self.engine.upgrade() {
            engine.set_cwd(PathBuf::from(cwd));
            engine.note_cwd_changed();
        }
        emit_worker_event_with(
            &self.core,
            &self.events,
            json!({ "type": "cwd_changed", "cwd": cwd }),
        );
        Ok(())
    }
}
