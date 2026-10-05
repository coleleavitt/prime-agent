//! The `ipython` tool: persistent Python REPL execution through a kernel —
//! the model-facing definition, result text composition, busy-kernel choice,
//! and the rlm bootstrap code. Kernel process management lives behind the
//! [`IpythonKernelProvisioner`] trait, owned by the kernel manager module.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use crate::tools::tool_definition::{
    AbortSignal, ExecutionMode, OnUpdate, ToolContentBlock, ToolDefinition, ToolExecutionResult,
    ToolUpdate,
};

/// Mime types the model context accepts as images.
pub const IMAGE_MIME_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

// Kernel execution types

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KernelErrorInfo {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

/// A media attachment loaded into context (e.g. by the attach-image skill).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelAttachment {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ExecuteStatus {
    #[default]
    Ok,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteResult {
    pub status: ExecuteStatus,
    pub stdout: String,
    pub stderr: String,
    pub result: Option<String>,
    pub duration_ms: Option<u64>,
    /// Output that arrived without this cell's id, shown separately.
    pub background_output: Option<String>,
    pub error: Option<KernelErrorInfo>,
    pub attachments: Vec<KernelAttachment>,
    /// Agent messages sent from this cell, in order (TS
    /// `sentAgentMessages` on the tool-result details).
    pub sent_agent_messages: Vec<crate::kernel::shared::KernelSentAgentMessage>,
    /// The `bash()` commands this cell started, summarized for display
    /// (`bashCommands` on the tool-result details).
    pub bash_commands: Option<crate::kernel::shared::KernelBashCommands>,
    /// The `bash()` commands that finished while the cell ran, with exit
    /// codes; reported to observers as the `bashCommands` host fact.
    pub executed_bash_commands: Vec<crate::kernel::shared::KernelExecutedBashCommand>,
    /// The per-cell execution timeout fired and interrupted the cell.
    pub timed_out: bool,
    /// The cell ignored the interrupt: the kernel is still running it.
    pub kernel_unresponsive: bool,
}

/// The wire form of one sent agent message (TS `KernelSentAgentMessage`):
/// `id`, `message`, `deliveryStatus`, `receiverRole` when present, and the
/// `target` endpoint (`sessionName` only when present).
#[must_use]
pub fn sent_agent_message_json(
    sent: &crate::kernel::shared::KernelSentAgentMessage,
) -> serde_json::Value {
    use crate::kernel::shared::{SentAgentMessageTarget, SentDeliveryStatus};
    let crate::kernel::shared::KernelSentAgentMessage {
        id,
        message,
        delivery_status,
        receiver_role,
        target:
            SentAgentMessageTarget {
                active_session_id,
                session_id,
                session_name,
            },
    } = sent;
    let delivery = match delivery_status {
        SentDeliveryStatus::Delivered => "delivered",
        SentDeliveryStatus::Queued => "queued",
        SentDeliveryStatus::Digest => "digest",
    };
    let mut value = json!({
        "id": id,
        "message": message,
        "deliveryStatus": delivery,
        "target": {
            "activeSessionId": active_session_id,
            "sessionId": session_id,
        },
    });
    if let Some(role) = receiver_role {
        value["receiverRole"] = json!(role.as_str());
    }
    if let Some(name) = session_name {
        value["target"]["sessionName"] = json!(name);
    }
    value
}

/// The kernel is still running a previously interrupted cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelBusyAfterInterruptError {
    pub message: String,
}

impl std::fmt::Display for KernelBusyAfterInterruptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Default for KernelBusyAfterInterruptError {
    fn default() -> Self {
        Self {
            message: "The Python kernel is still running the previously interrupted cell. Wait and try again, or kill the kernel to start fresh.".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum KernelExecError {
    /// The kernel is busy with a previously interrupted cell.
    BusyAfterInterrupt(KernelBusyAfterInterruptError),
    /// The kernel process died running the cell; the next call gets a
    /// fresh kernel.
    KernelExited(crate::kernel::shared::KernelExitedError),
    /// Any other kernel failure.
    Other(anyhow::Error),
}

impl KernelExecError {
    /// True when this error is a busy-after-interrupt error.
    #[must_use]
    pub fn is_busy_after_interrupt(&self) -> bool {
        matches!(self, KernelExecError::BusyAfterInterrupt(_))
    }

    /// The model-facing message.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            KernelExecError::BusyAfterInterrupt(err) => err.message.clone(),
            KernelExecError::KernelExited(err) => err.to_string(),
            // The full context chain, not just the outermost layer: a bare
            // top-level message hides the actual cause of kernel failures.
            KernelExecError::Other(err) => format!("{err:#}"),
        }
    }
}

pub type StreamFn<'a> = Option<&'a (dyn Fn(&str, &'static str) + Send + Sync)>;

pub type LateSentAgentMessageHandler =
    std::sync::Arc<dyn Fn(&str, crate::kernel::shared::KernelSentAgentMessage) + Send + Sync>;

pub struct KernelExecuteOptions<'a> {
    pub signal: Option<AbortSignal>,
    /// Interrupt the cell after this many milliseconds of kernel time
    /// (time spent waiting on host requests such as sub-agent runs does not
    /// count). `None` runs unbounded.
    pub timeout_ms: Option<u64>,
    /// Streams cell output while the cell runs.
    pub on_stream: StreamFn<'a>,
    pub on_late_sent_agent_message: Option<crate::kernel::shared::LateSentAgentMessageCallback>,
}

type ExecuteCellFuture =
    Pin<Box<dyn Future<Output = Result<ExecuteResult, KernelExecError>> + Send>>;

/// A running kernel able to execute code cells.
pub trait KernelExecutor: Send + Sync {
    /// Execute one cell in the persistent kernel.
    fn execute(&self, code: &str, options: KernelExecuteOptions<'_>) -> ExecuteCellFuture;
}

/// Startup progress handler (TS: `KernelBootstrapProgressHandler`).
pub type BootstrapProgressHandler = Arc<dyn Fn(&str) + Send + Sync>;

type EnsureFuture = Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn KernelExecutor>>> + Send>>;

/// Owns the lazy create+start+bootstrap of one session's Python kernel.
/// Implementations memoize one running kernel (concurrent `ensure` calls await
/// the same in-flight startup; a failed startup clears the memo), and `kill`
/// terminates the kernel losing all in-memory state. Object-safe on purpose.
pub trait IpythonKernelProvisioner: Send + Sync {
    /// Start (or reuse) the kernel; resolves once it is ready to execute.
    fn ensure(
        &self,
        on_progress: Option<BootstrapProgressHandler>,
        signal: Option<AbortSignal>,
    ) -> EnsureFuture;

    /// Kill the kernel immediately, losing all in-memory state.
    fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    /// The unexpected exit of a kernel that was replaced by a fresh one and
    /// not yet reported, handed out once (the one-time restart notice).
    /// Provisioners whose kernels cannot die on their own keep the default.
    fn take_unreported_exit(&self) -> Option<crate::kernel::shared::KernelUnexpectedExit> {
        None
    }
}

// Busy-kernel choice UI

pub const BUSY_KERNEL_WAIT_CHOICE: &str = "Wait and preserve state";
pub const BUSY_KERNEL_KILL_CHOICE: &str = "Kill kernel and restart";

pub fn busy_kernel_prompt() -> String {
    [
        "Interrupted Python cell is still running",
        "Ctrl+C sent an interrupt, but the previous cell has not stopped yet. A new command cannot start until it finishes.",
        "Waiting preserves the current kernel state. Killing restarts the kernel and loses in-memory variables, imports, and running tasks.",
    ]
    .join("\n")
}

/// The one-time notice on the first cell after the kernel died on its own
/// and was replaced (TS `kernelCrashRecoveryNotice`).
#[must_use]
pub fn kernel_crash_recovery_notice(exit: &crate::kernel::shared::KernelUnexpectedExit) -> String {
    format!(
        "<ipython_kernel_reset>\nThe Python kernel was restarted after it exited unexpectedly ({}) at {}; variables were revived from the last snapshot, but imports, live handles, open resources, and background tasks from before are gone.\n</ipython_kernel_reset>",
        exit.cause(),
        crate::session::manager::format_iso(i64::try_from(exit.at_ms).unwrap_or(i64::MAX)),
    )
}

/// The per-cell execution timeout used when nothing configures one: ten
/// minutes of kernel time, far above a full test suite run.
pub const DEFAULT_IPYTHON_CELL_TIMEOUT_MS: u64 = 600_000;

/// Env override for the per-cell execution timeout, in milliseconds; `0`
/// disables it.
pub const IPYTHON_CELL_TIMEOUT_ENV: &str = "PRIME_AGENT_IPYTHON_TIMEOUT_MS";

/// The per-cell execution timeout from `raw` (the env value): unset or
/// unparsable falls back to the default, `0` disables the timeout.
#[must_use]
pub fn resolve_cell_timeout_ms(raw: Option<&str>) -> Option<u64> {
    match raw.map(str::trim).map(str::parse::<u64>) {
        Some(Ok(0)) => None,
        Some(Ok(ms)) => Some(ms),
        Some(Err(_)) | None => Some(DEFAULT_IPYTHON_CELL_TIMEOUT_MS),
    }
}

/// The notice on a cell the execution timeout interrupted.
#[must_use]
pub fn cell_timeout_notice(timeout_ms: u64, kernel_killed: bool) -> String {
    let seconds = timeout_ms.div_ceil(1_000);
    let outcome = if kernel_killed {
        "The kernel did not stop after the interrupt, so it was killed. A fresh kernel starts on the next call: variables come back from the last snapshot, but imports, live handles, async tasks, and open resources from before are gone; recreate them before using them."
    } else {
        "The kernel stopped the cell; its state is preserved."
    };
    format!(
        "<ipython_timeout>\nThe cell exceeded the {seconds}s execution timeout and was interrupted (time spent waiting on sub-agents does not count). {outcome} Common causes: a subprocess without timeout=, a git command waiting for an editor (set GIT_EDITOR=true), a network call without a timeout. Retry with a bounded command, or run long work in the background. The limit is set by {IPYTHON_CELL_TIMEOUT_ENV}.\n</ipython_timeout>"
    )
}

/// The notice on a cancelled cell whose kernel ignored the interrupt and
/// was killed so later calls do not find it busy.
#[must_use]
pub fn unresponsive_kernel_killed_notice() -> &'static str {
    "<ipython_kernel_reset>\nThe cancelled cell did not stop after the interrupt, so the Python kernel was killed. A fresh kernel starts on the next call: variables come back from the last snapshot, but imports, live handles, async tasks, and open resources from before are gone; recreate them before using them.\n</ipython_kernel_reset>"
}

pub fn kernel_restart_notice() -> &'static str {
    "<ipython_kernel_reset>\nThe Python kernel was restarted after a previous interrupted cell kept running. Variables, imports, async tasks, and open resources from before the restart are no longer available; recreate them before using them.\n</ipython_kernel_reset>"
}

/// The UI surface the ipython tool needs from the host session (the TS tool context).
pub trait IpythonToolUi: Send + Sync {
    /// Prompt the user to choose between `choices`.
    fn select(
        &self,
        prompt: &str,
        choices: &[&str],
        signal: Option<&AbortSignal>,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send>>;

    /// Show/hide a transient working message.
    fn set_working_message(&self, message: Option<&str>);
}

// Tool definition

/// Turn kernel image attachments into image blocks; non-images are dropped.
pub fn image_blocks_from_attachments(attachments: &[KernelAttachment]) -> Vec<ToolContentBlock> {
    attachments
        .iter()
        .filter(|a| IMAGE_MIME_TYPES.contains(&a.mime_type.as_str()))
        .map(|a| ToolContentBlock::Image {
            data: a.data.clone(),
            mime_type: a.mime_type.clone(),
        })
        .collect()
}

fn format_execute_text(result: &ExecuteResult, background_output: Option<&str>) -> String {
    let mut text = result.stdout.clone();
    if !result.stderr.is_empty() {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(&result.stderr);
    }
    if let Some(result_text) = &result.result {
        text.push_str(if text.is_empty() { "" } else { "\n" });
        text.push_str(result_text);
    }
    if result.status == ExecuteStatus::Error {
        if let Some(error) = &result.error {
            text.push_str(if text.is_empty() { "" } else { "\n" });
            text.push_str(&error.traceback.join("\n"));
        }
    }
    if let Some(background) = background_output {
        let separator = if text.is_empty() { "" } else { "\n" };
        let _ = write!(
            text,
            "{separator}[background output (unattributed)]\n{background}"
        );
    }
    text
}

async fn execute_with_busy_kernel_choice(
    provisioner: &dyn IpythonKernelProvisioner,
    report_startup_progress: &BootstrapProgressHandler,
    code: &str,
    execute: KernelExecuteOptions<'_>,
    on_working_message: &(dyn Fn(Option<&str>) + Send + Sync),
    ui: Option<&Arc<dyn IpythonToolUi>>,
    kernel_restarted: &mut bool,
) -> Result<ExecuteResult, KernelExecError> {
    loop {
        let manager = provisioner
            .ensure(
                Some(report_startup_progress.clone()),
                execute.signal.clone(),
            )
            .await
            .map_err(KernelExecError::Other)?;
        let result = manager
            .execute(
                code,
                KernelExecuteOptions {
                    signal: execute.signal.clone(),
                    timeout_ms: execute.timeout_ms,
                    on_stream: execute.on_stream,
                    on_late_sent_agent_message: execute.on_late_sent_agent_message.clone(),
                },
            )
            .await;
        match result {
            Ok(result) => return Ok(result),
            Err(err) => {
                let aborted = execute
                    .signal
                    .as_ref()
                    .is_some_and(tokio_util::sync::CancellationToken::is_cancelled);
                if !err.is_busy_after_interrupt() || aborted {
                    return Err(err);
                }
                // No UI (headless): nobody can choose to wait, and every
                // later call would hit the same wedged cell. Replace the
                // kernel (TS #2135).
                let Some(ui) = ui else {
                    on_working_message(Some("Restarting Python kernel..."));
                    provisioner.kill().await;
                    *kernel_restarted = true;
                    continue;
                };
                let choice = ui
                    .select(
                        &busy_kernel_prompt(),
                        &[BUSY_KERNEL_WAIT_CHOICE, BUSY_KERNEL_KILL_CHOICE],
                        execute.signal.as_ref(),
                    )
                    .await;
                match choice.as_deref() {
                    Some(BUSY_KERNEL_WAIT_CHOICE) => {
                        on_working_message(Some("Waiting for Python kernel..."));
                    }
                    Some(BUSY_KERNEL_KILL_CHOICE) => {
                        on_working_message(Some("Restarting Python kernel..."));
                        provisioner.kill().await;
                        *kernel_restarted = true;
                    }
                    _ => return Err(err),
                }
            }
        }
    }
}

pub struct IpythonToolOptions {
    /// Shared provisioner owning the kernel lifecycle.
    pub provisioner: Arc<dyn IpythonKernelProvisioner>,
    /// UI surface; `None` in headless sessions.
    pub ui: Option<Arc<dyn IpythonToolUi>>,
    pub on_late_sent_agent_message: Option<LateSentAgentMessageHandler>,
    /// Per-cell execution timeout in milliseconds; `None` runs unbounded.
    pub cell_timeout_ms: Option<u64>,
}

pub fn ipython_tool_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["code"],
        "properties": {
            "code": {
                "type": "string",
                "description": "Python code to execute in the persistent Python REPL. Use the target project's own environment for project imports, tests, scripts, CLIs, and dependency checks instead of direct kernel imports."
            }
        }
    })
}

pub fn ipython_tool_description() -> &'static str {
    "Execute Python code in a persistent Python REPL. Top-level `await` is supported. Variables, imports, and loaded data persist across calls, and are revived on a best-effort basis when a session is resumed (objects that cannot be serialized are dropped and reported). Run shell commands with `bash('cmd')` / `await bash('cmd')`. Project imports, tests, scripts, CLIs, and dependency checks should run through the target project's own environment."
}

#[tracing::instrument(
    level = "debug",
    name = "tool_ipython_execute",
    skip(options, on_update, on_late_sent_agent_message)
    fields(code),
)]
pub async fn execute_ipython(
    options: &IpythonToolOptions,
    code: &str,
    signal: Option<AbortSignal>,
    on_update: Option<OnUpdate>,
    on_late_sent_agent_message: Option<crate::kernel::shared::LateSentAgentMessageCallback>,
) -> anyhow::Result<ToolExecutionResult> {
    let set_tool_working_message = |message: Option<&str>| {
        if let Some(ui) = &options.ui {
            ui.set_working_message(message);
        }
    };

    let ui = options.ui.clone();
    let report_startup_progress: BootstrapProgressHandler = {
        let on_update = on_update.clone();
        let ui = ui.clone();
        Arc::new(move |message: &str| {
            if let Some(ui) = &ui {
                ui.set_working_message(Some(message));
            }
            if let Some(on_update) = &on_update {
                on_update(ToolUpdate {
                    content: vec![ToolContentBlock::text(message.to_string())],
                    details: Some(json!({ "status": "starting" })),
                });
            }
        })
    };

    // Stream cell output chunks as in-flight updates (TS onStream).
    let stream_update = |chunk: &str, _stream: &'static str| {
        if let Some(on_update) = &on_update {
            on_update(ToolUpdate {
                content: vec![ToolContentBlock::text(chunk.to_string())],
                details: Some(json!({ "status": "ok" })),
            });
        }
    };

    let started = std::time::Instant::now();
    let mut kernel_restarted = false;
    let result = execute_with_busy_kernel_choice(
        options.provisioner.as_ref(),
        &report_startup_progress,
        code,
        KernelExecuteOptions {
            signal,
            timeout_ms: options.cell_timeout_ms,
            on_stream: Some(&stream_update),
            on_late_sent_agent_message,
        },
        &|message| {
            set_tool_working_message(message);
        },
        options.ui.as_ref(),
        &mut kernel_restarted,
    )
    .await;

    set_tool_working_message(None);

    let r = match result {
        Ok(r) => r,
        // The interpreter died running this cell. Never re-run the cell (a
        // cell that crashes the interpreter would loop); the provisioner
        // serves a fresh kernel on the next call, so say what happened.
        Err(KernelExecError::KernelExited(error)) => {
            return Ok(kernel_crash_result(&error, started, kernel_restarted));
        }
        Err(err) => return Err(anyhow::anyhow!("{}", err.message())),
    };

    // Escalate past an ignored interrupt: a wedged kernel would answer every
    // later call with busy-after-interrupt. A timeout always escalates (no
    // one chose to wait); a cancel escalates when no UI offers the
    // wait/kill choice (TS #2135).
    let kill_unresponsive = r.kernel_unresponsive && (r.timed_out || options.ui.is_none());
    if kill_unresponsive {
        options.provisioner.kill().await;
    }
    let mut text = format_execute_text(&r, r.background_output.as_deref());
    let escalation_notice = if r.timed_out {
        options
            .cell_timeout_ms
            .map(|ms| cell_timeout_notice(ms, kill_unresponsive))
    } else if kill_unresponsive {
        Some(unresponsive_kernel_killed_notice().to_string())
    } else {
        None
    };
    if let Some(notice) = escalation_notice {
        text = if text.is_empty() {
            notice
        } else {
            format!("{text}\n\n{notice}")
        };
    }
    if kernel_restarted {
        text = if text.is_empty() {
            kernel_restart_notice().to_string()
        } else {
            format!("{}\n\n{}", kernel_restart_notice(), text)
        };
    }
    if let Some(exit) = options.provisioner.take_unreported_exit() {
        let notice = kernel_crash_recovery_notice(&exit);
        text = if text.is_empty() {
            notice
        } else {
            format!("{notice}\n\n{text}")
        };
    }

    let image_blocks = image_blocks_from_attachments(&r.attachments);
    let mut content = vec![ToolContentBlock::text(text)];
    content.extend(image_blocks);

    let mut details = json!({
        "status": match r.status {
            ExecuteStatus::Ok => "ok",
            ExecuteStatus::Error => "error",
            ExecuteStatus::Aborted => "aborted",
        },
        "kernelRestarted": kernel_restarted,
    });
    if r.timed_out {
        details["timedOut"] = json!(true);
    }
    if kill_unresponsive {
        details["kernelKilled"] = json!(true);
    }
    if let Some(duration) = r.duration_ms {
        details["durationMs"] = json!(duration);
    }
    if !r.stdout.is_empty() {
        details["stdout"] = json!(r.stdout);
    }
    if !r.stderr.is_empty() {
        details["stderr"] = json!(r.stderr);
    }
    if let Some(result_text) = &r.result {
        details["result"] = json!(result_text);
    }
    if let Some(bash) = &r.bash_commands {
        details["bashCommands"] = json!({
            "first": bash.first,
            "count": bash.count,
            "lines": bash.lines,
        });
    }
    if let Some(background) = &r.background_output {
        details["backgroundOutput"] = json!(background);
    }
    if let Some(error) = &r.error {
        details["error"] = json!({
            "ename": error.ename,
            "evalue": error.evalue,
            "traceback": error.traceback,
        });
        details["errorEname"] = json!(error.ename);
    }
    if !r.sent_agent_messages.is_empty() {
        details["sentAgentMessages"] = json!(r
            .sent_agent_messages
            .iter()
            .map(sent_agent_message_json)
            .collect::<Vec<_>>());
    }

    // Host facts for in-process observers (never persisted): the cell's
    // finished `bash()` commands with exit codes, TS `bashCommands` shape.
    let host_facts = if r.executed_bash_commands.is_empty() {
        serde_json::Value::Null
    } else {
        json!({
            "bashCommands": r
                .executed_bash_commands
                .iter()
                .map(crate::kernel::shared::KernelExecutedBashCommand::to_json)
                .collect::<Vec<_>>(),
        })
    };

    Ok(ToolExecutionResult {
        content,
        details: Some(details),
        is_error: r.status == ExecuteStatus::Error || r.status == ExecuteStatus::Aborted,
        host_facts,
    })
}

/// The tool result for a cell whose kernel died: the exit (code/signal,
/// request, stderr tail) as an error the model reads, plus the structured
/// `kernelCrashed` details (TS `kernelCrashDetails`).
fn kernel_crash_result(
    error: &crate::kernel::shared::KernelExitedError,
    started: std::time::Instant,
    kernel_restarted: bool,
) -> ToolExecutionResult {
    let exit = &error.exit;
    ToolExecutionResult {
        content: vec![ToolContentBlock::text(error.to_string())],
        details: Some(json!({
            "durationMs": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "status": "error",
            "errorEname": "KernelExitedError",
            "kernelRestarted": kernel_restarted,
            "kernelCrashed": {
                "exitCode": exit.exit_code,
                "signal": exit.signal,
                "requestId": exit.request_id,
                "stderrTail": exit.stderr_tail,
            },
        })),
        is_error: true,
        host_facts: serde_json::Value::Null,
    }
}

/// The `ipython` tool definition: exact name, schema, and description.
#[must_use]
pub fn create_ipython_tool_definition(_cwd: &str, options: IpythonToolOptions) -> ToolDefinition {
    let options = Arc::new(options);
    let execute: crate::tools::tool_definition::ExecuteFn = {
        let options = options;
        Arc::new(move |tool_call_id, params, signal, on_update| {
            let options = options.clone();
            let on_late_sent_agent_message =
                options.on_late_sent_agent_message.as_ref().map(|handler| {
                    let handler = std::sync::Arc::clone(handler);
                    let tool_call_id = tool_call_id.to_string();
                    std::sync::Arc::new(move |message| handler(&tool_call_id, message))
                        as crate::kernel::shared::LateSentAgentMessageCallback
                });
            Box::pin(async move {
                let code = params
                    .get("code")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("ipython tool requires a code string"))?
                    .to_string();
                execute_ipython(
                    &options,
                    &code,
                    signal,
                    on_update,
                    on_late_sent_agent_message,
                )
                .await
            })
        })
    };
    ToolDefinition {
        name: "ipython".to_string(),
        label: "ipython".to_string(),
        description: ipython_tool_description().to_string(),
        prompt_snippet:
            "ipython - persistent Python REPL for code, state, and bash() orchestration".to_string(),
        // The kernel is single-threaded; calls must not run in parallel.
        execution_mode: Some(ExecutionMode::Sequential),
        parameters: ipython_tool_schema(),
        prepare_arguments: None,
        execute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_agent_message_json_matches_ts_wire_shape() {
        let sent = crate::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_1".to_string(),
            message: "Ping.\nThen report back.".to_string(),
            delivery_status: crate::kernel::shared::SentDeliveryStatus::Delivered,
            receiver_role: Some(crate::kernel::shared::ReceiverRole::Parent),
            target: crate::kernel::shared::SentAgentMessageTarget {
                active_session_id: "worker-active".to_string(),
                session_id: "worker-session".to_string(),
                session_name: Some("Worker".to_string()),
            },
        };
        assert_eq!(
            sent_agent_message_json(&sent),
            json!({
                "id": "agentmsg_1",
                "message": "Ping.\nThen report back.",
                "deliveryStatus": "delivered",
                "receiverRole": "parent",
                "target": {
                    "activeSessionId": "worker-active",
                    "sessionId": "worker-session",
                    "sessionName": "Worker",
                },
            })
        );
        // The queued receipt without a role or session name omits both.
        let queued = crate::kernel::shared::KernelSentAgentMessage {
            id: "agentmsg_2".to_string(),
            message: "Ping.".to_string(),
            delivery_status: crate::kernel::shared::SentDeliveryStatus::Queued,
            receiver_role: None,
            target: crate::kernel::shared::SentAgentMessageTarget {
                active_session_id: "a1".to_string(),
                session_id: "s1".to_string(),
                session_name: None,
            },
        };
        assert_eq!(
            sent_agent_message_json(&queued),
            json!({
                "id": "agentmsg_2",
                "message": "Ping.",
                "deliveryStatus": "queued",
                "target": { "activeSessionId": "a1", "sessionId": "s1" },
            })
        );
    }

    #[test]
    fn the_cell_timeout_defaults_to_ten_minutes_and_zero_disables_it() {
        assert_eq!(
            [None, Some("90000"), Some(" 0 "), Some("soon")].map(resolve_cell_timeout_ms),
            [Some(600_000), Some(90_000), None, Some(600_000)]
        );
    }

    /// Serves scripted cell outcomes; a crash outcome "replaces" the
    /// kernel, leaving its exit unreported like the real provisioner.
    struct CrashingProvisioner {
        outcomes: std::sync::Mutex<
            std::collections::VecDeque<
                Result<ExecuteResult, crate::kernel::shared::KernelExitedError>,
            >,
        >,
        unreported: Arc<std::sync::Mutex<Option<crate::kernel::shared::KernelUnexpectedExit>>>,
    }

    struct ScriptedExecutor(Result<ExecuteResult, crate::kernel::shared::KernelExitedError>);

    impl KernelExecutor for ScriptedExecutor {
        fn execute(&self, _code: &str, _options: KernelExecuteOptions<'_>) -> ExecuteCellFuture {
            Box::pin(std::future::ready(
                self.0.clone().map_err(KernelExecError::KernelExited),
            ))
        }
    }

    impl IpythonKernelProvisioner for CrashingProvisioner {
        fn ensure(
            &self,
            _on_progress: Option<BootstrapProgressHandler>,
            _signal: Option<AbortSignal>,
        ) -> EnsureFuture {
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted outcome");
            if let Err(crashed) = &outcome {
                *self.unreported.lock().unwrap() = Some(crashed.exit.clone());
            }
            let executor: Box<dyn KernelExecutor> = Box::new(ScriptedExecutor(outcome));
            Box::pin(std::future::ready(Ok(executor)))
        }

        fn kill(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(std::future::ready(()))
        }

        fn take_unreported_exit(&self) -> Option<crate::kernel::shared::KernelUnexpectedExit> {
            self.unreported.lock().unwrap().take()
        }
    }

    fn printed(stdout: &str) -> ExecuteResult {
        ExecuteResult {
            stdout: stdout.to_string(),
            ..ExecuteResult::default()
        }
    }

    #[tokio::test]
    async fn a_kernel_crash_reports_the_exit_and_the_next_cell_carries_a_one_time_notice() {
        let exit = crate::kernel::shared::KernelUnexpectedExit {
            exit_code: Some(7),
            signal: None,
            request_id: Some("req-1".to_string()),
            request_type: Some("execute"),
            stderr_tail: "boom\n".to_string(),
            // 2026-01-02T03:04:05.006Z
            at_ms: 1_767_323_045_006,
        };
        let options = IpythonToolOptions {
            provisioner: Arc::new(CrashingProvisioner {
                outcomes: std::sync::Mutex::new(
                    [
                        Err(crate::kernel::shared::KernelExitedError { exit: exit.clone() }),
                        Ok(printed("after\n")),
                        Ok(printed("again\n")),
                    ]
                    .into(),
                ),
                unreported: Arc::default(),
            }),
            ui: None,
            on_late_sent_agent_message: None,
            cell_timeout_ms: None,
        };

        let crashed = execute_ipython(&options, "import os; os._exit(7)", None, None, None)
            .await
            .unwrap();
        let mut details = crashed.details.clone().unwrap();
        details.as_object_mut().unwrap().remove("durationMs");
        assert_eq!(
            (crashed.content, details, crashed.is_error),
            (
                vec![ToolContentBlock::text(
                    "Kernel process exited unexpectedly (exit code 7) while serving execute request req-1. A fresh kernel starts on the next call: variables come back from the last snapshot, imports and live handles (bash, rlm, skills) are re-bootstrapped; background tasks and open resources are lost.\nKernel stderr tail:\nboom"
                        .to_string()
                )],
                json!({
                    "status": "error",
                    "errorEname": "KernelExitedError",
                    "kernelRestarted": false,
                    "kernelCrashed": {
                        "exitCode": 7,
                        "signal": null,
                        "requestId": "req-1",
                        "stderrTail": "boom\n",
                    },
                }),
                true,
            )
        );

        let after = execute_ipython(&options, "print('after')", None, None, None)
            .await
            .unwrap();
        assert_eq!(
            after.content,
            vec![ToolContentBlock::text(
                "<ipython_kernel_reset>\nThe Python kernel was restarted after it exited unexpectedly (exit code 7) at 2026-01-02T03:04:05.006Z; variables were revived from the last snapshot, but imports, live handles, open resources, and background tasks from before are gone.\n</ipython_kernel_reset>\n\nafter\n"
                    .to_string()
            )]
        );

        // One time only.
        let again = execute_ipython(&options, "print('again')", None, None, None)
            .await
            .unwrap();
        assert_eq!(
            again.content,
            vec![ToolContentBlock::text("again\n".to_string())]
        );
    }
}
