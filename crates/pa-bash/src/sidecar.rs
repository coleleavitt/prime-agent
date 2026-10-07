//! The standalone transport: `prime-agent --prime-agent-bash-host` serves the
//! `bash.*` requests of one kernel process over its stdin/stdout, for a
//! runtime that runs outside a Prime Agent host (tests, scripts). Each line
//! in is `{"id": <string>, "data": <request>}`, each line out
//! `{"id": <same>, "data": <reply>}`; requests run concurrently (a
//! `bash.follow` waits for its job's next event). When stdin closes (the
//! kernel process exited or died) every live job's group is killed.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::runner::JobTable;
use crate::service::handle;

/// Serve requests from stdin until it closes. Returns the process exit code.
#[must_use]
pub fn serve_stdio() -> i32 {
    let table = Arc::new(JobTable::new());
    let out = Arc::new(Mutex::new(std::io::stdout()));
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
        let data = frame.get("data").cloned().unwrap_or(Value::Null);
        let table = Arc::clone(&table);
        let out = Arc::clone(&out);
        std::thread::spawn(move || {
            let reply = handle(&table, &data);
            let mut text = json!({"id": id, "data": reply}).to_string();
            text.push('\n');
            let mut out = out.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        });
    }
    table.kill_all();
    0
}
