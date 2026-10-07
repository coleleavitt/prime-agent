//! The standalone transport: `prime-agent --prime-agent-bash-host` serves the
//! `bash.*` requests of one kernel process over its stdin/stdout, for a
//! runtime that runs outside a Prime Agent host (tests, scripts). Each line
//! in is `{"id": <string>, "data": <request>}`, each line out
//! `{"id": <same>, "data": <reply>}`; requests run concurrently (a
//! `bash.follow` waits for its job's next event). A line
//! `{"id": <string>, "cancel": true}` cancels that pending request (a
//! `bash.run` kills its job and still answers), like the host's
//! `host_cancel`. When stdin closes (the kernel process exited or died) every
//! live job's group is killed.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{json, Value};

use crate::run::RunCancel;
use crate::runner::JobTable;
use crate::sandbox::JobSandbox;
use crate::service::handle_cancellable;

/// Serve requests from stdin until it closes, starting every process under
/// `sandbox`. Returns the process exit code.
#[must_use]
pub fn serve_stdio(sandbox: JobSandbox) -> i32 {
    let table = Arc::new(JobTable::new());
    table.set_sandbox(sandbox);
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Arc<Mutex<HashMap<String, RunCancel>>> = Arc::default();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        let key = id.to_string();
        if frame.get("cancel").and_then(Value::as_bool) == Some(true) {
            let cancel = pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&key)
                .cloned();
            if let Some(cancel) = cancel {
                cancel.cancel();
            }
            continue;
        }
        let data = frame.get("data").cloned().unwrap_or(Value::Null);
        let cancel = RunCancel::default();
        pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), cancel.clone());
        let table = Arc::clone(&table);
        let out = Arc::clone(&out);
        let pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            let reply = handle_cancellable(&table, &data, &cancel);
            pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&key);
            let mut text = json!({"id": id, "data": reply}).to_string();
            text.push('\n');
            let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        });
    }
    table.kill_all();
    0
}
