//! The `persistent_shells.*` kernel host requests (the write/attach side of
//! `bash(..., persistent=True)` and `rlm.persistent_shells()`): `spawn`
//! detaches a shell into `<agent dir>/persistent-shells/` via
//! [`crate::persistent_shells::spawn`], and `list`/`tail`/`kill` resolve the
//! records this session left through the same module's reader. Registered only
//! on a supervisor-backed worker with a registered engine arc; elsewhere the
//! kernel keeps its "persistent shells need a saved session" surface. Records
//! are keyed by the DURABLE session id (`session.session_id()`), the same id
//! the saved-session catalog looks them up by, not the worker's short active
//! id. The daemon READ side ([`crate::persistent_shells`]) lists the same
//! records for the agents view and the saved-session rows.

use std::path::PathBuf;

use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_types::sync::MutexExt;
use serde_json::{Value, json};

use super::AgentSessionEngine;

impl AgentSessionEngine {
    /// Register `persistent_shells.{spawn,list,tail,kill}`. A None supervisor
    /// link (print/RPC/no-daemon) or an unregistered engine arc leaves them
    /// unregistered, so persistent shells are offered only where their records
    /// can outlive the session and be keyed by its durable id.
    pub(crate) fn register_persistent_shells_host_handlers(
        &self,
        handlers: &mut HostRequestHandlers,
    ) {
        if self.config.supervisor_link.is_none() {
            return;
        }
        let Some(weak) = self.self_weak.lock_or_recover().clone() else {
            return;
        };
        let agent_dir = self.config.agent_dir.clone();

        {
            let weak = weak.clone();
            let agent_dir = agent_dir.clone();
            handlers.register(
                "persistent_shells.spawn",
                host_handler(move |payload| {
                    let weak = weak.clone();
                    let agent_dir = agent_dir.clone();
                    Box::pin(async move {
                        let data = payload.data;
                        let script = data
                            .get("script")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                anyhow::anyhow!("persistent_shells.spawn requires a script")
                            })?
                            .to_string();
                        let command = data
                            .get("command")
                            .and_then(Value::as_str)
                            .unwrap_or(script.as_str())
                            .to_string();
                        let shell = data
                            .get("shell")
                            .and_then(Value::as_str)
                            .unwrap_or("bash")
                            .to_string();
                        let cwd = data
                            .get("cwd")
                            .and_then(Value::as_str)
                            .map_or_else(|| agent_dir.clone(), PathBuf::from);
                        let env: Vec<(String, String)> = data
                            .get("env")
                            .and_then(Value::as_object)
                            .map(|map| {
                                map.iter()
                                    .filter_map(|(key, value)| {
                                        value.as_str().map(|value| (key.clone(), value.to_string()))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let Some(engine) = weak.upgrade() else {
                            anyhow::bail!("the session ended before the persistent shell spawned");
                        };
                        let Some(core) = engine.session.lock().await.clone() else {
                            anyhow::bail!("the session's kernel is not active");
                        };
                        let session_id = core.session.session_id().await;
                        tokio::task::spawn_blocking(move || {
                            crate::persistent_shells::spawn(
                                &agent_dir,
                                &session_id,
                                &shell,
                                &script,
                                &command,
                                &cwd,
                                &env,
                            )
                        })
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("persistent shell spawn did not finish: {error}")
                        })?
                        .map_err(anyhow::Error::msg)
                    })
                }),
            );
        }

        {
            let agent_dir = agent_dir.clone();
            handlers.register(
                "persistent_shells.list",
                host_handler(move |_payload| {
                    let weak = weak.clone();
                    let agent_dir = agent_dir.clone();
                    Box::pin(async move {
                        let Some(engine) = weak.upgrade() else {
                            anyhow::bail!("the session ended before its persistent shells listed");
                        };
                        let Some(core) = engine.session.lock().await.clone() else {
                            anyhow::bail!("the session's kernel is not active");
                        };
                        let session_id = core.session.session_id().await;
                        let shells = tokio::task::spawn_blocking(move || {
                            crate::persistent_shells::shell_rows(&agent_dir)
                                .into_iter()
                                .filter(|row| {
                                    row.get("sessionId").and_then(Value::as_str)
                                        == Some(session_id.as_str())
                                })
                                .collect::<Vec<_>>()
                        })
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("persistent shell list did not finish: {error}")
                        })?;
                        Ok(json!({ "shells": shells }))
                    })
                }),
            );
        }

        {
            let agent_dir = agent_dir.clone();
            handlers.register(
                "persistent_shells.tail",
                host_handler(move |payload| {
                    let agent_dir = agent_dir.clone();
                    Box::pin(async move {
                        let data = payload.data;
                        let id = data
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                anyhow::anyhow!("persistent_shells.tail requires an id")
                            })?
                            .to_string();
                        let lines = usize::try_from(
                            data.get("lines").and_then(Value::as_u64).unwrap_or(50),
                        )
                        .unwrap_or(50);
                        let tail = tokio::task::spawn_blocking(move || {
                            crate::persistent_shells::output_tail(&agent_dir, &id, lines)
                        })
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("persistent shell tail did not finish: {error}")
                        })?
                        .map_err(anyhow::Error::msg)?;
                        Ok(json!({ "output": tail }))
                    })
                }),
            );
        }

        {
            let agent_dir = agent_dir.clone();
            handlers.register(
                "persistent_shells.output",
                host_handler(move |payload| {
                    let agent_dir = agent_dir.clone();
                    Box::pin(async move {
                        let data = payload.data;
                        let id = data
                            .get("id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                anyhow::anyhow!("persistent_shells.output requires an id")
                            })?
                            .to_string();
                        let output = tokio::task::spawn_blocking(move || {
                            crate::persistent_shells::output_raw(&agent_dir, &id)
                        })
                        .await
                        .map_err(|error| {
                            anyhow::anyhow!("persistent shell output did not finish: {error}")
                        })?
                        .map_err(anyhow::Error::msg)?;
                        Ok(json!({ "output": output }))
                    })
                }),
            );
        }

        handlers.register(
            "persistent_shells.kill",
            host_handler(move |payload| {
                let agent_dir = agent_dir.clone();
                Box::pin(async move {
                    let data = payload.data;
                    let id = data
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("persistent_shells.kill requires an id"))?
                        .to_string();
                    let killed = tokio::task::spawn_blocking(move || {
                        crate::persistent_shells::kill_shell(&agent_dir, &id)
                    })
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("persistent shell kill did not finish: {error}")
                    })?
                    .map_err(anyhow::Error::msg)?;
                    Ok(json!({ "killed": killed }))
                })
            }),
        );
    }
}
