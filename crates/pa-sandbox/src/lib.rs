//! Prime Sandboxes client for Prime Agent cloud delegation.
//!
//! Port of the direct-model Prime Sandboxes surface from the TS branch
//! `feat/direct-cloud-sandbox` (ground-truth REST + Connect clients,
//! cross-checked against the live `prime_sandboxes` SDK and the platform
//! backend wire contract), split into two slices:
//!
//! - the lifecycle half ([`PrimeSandboxClient`]): idempotent VM create,
//!   fetch, delete, and wait-until-running against the Prime platform
//!   REST API;
//! - the gateway half (also on [`PrimeSandboxClient`]): the per-sandbox
//!   gateway credential fetch and the authenticated exec, upload, and
//!   download data path, plus the `ConnectRPC` `command_session` stream
//!   client ([`CommandSessionClient`]) VM process execution uses.
//!
//! Safety contract (TS parity): no secret (API key, gateway token) ever
//! appears in an error message, URL, or `details` preview; every response
//! is strictly validated (a 200 with a malformed body is a typed
//! `invalid_response` error, never a silent default); upload and
//! download payloads are bounded; all URL segments derived from platform
//! data are validated before use; and the transport refuses redirects, so
//! an authenticated platform or gateway call never hops origins.
//! Everything is injected (API key, base URL, transport, auth source,
//! deadlines) so nothing is read from the environment or `~/.prime`.
//!
//! Out of scope for this crate (see the TS module headers for the full
//! contract): guest runtime orchestration — the resident guest daemon,
//! workspace snapshot and transfer, delegation records, tunnel bridging
//! (the TS `direct-cloud-service.ts` side) — and container (non-VM)
//! sandbox creates.

pub mod client;
pub mod command_session;
pub mod error;
pub mod gateway;
pub mod proto;
pub mod transport;
pub mod types;
pub mod vm_error;
pub mod vm_process;
pub mod vm_stream;

mod record;
mod vm_wire;
mod wire;

pub use client::{ClientOptions, DEFAULT_BASE_URL, PrimeSandboxClient};
pub use command_session::{
    CommandSessionEvent,
    CommandSpec,
    EndEvent,
    InputChannel,
    OutputChannel,
    PtySize,
    StartRequest,
    VmSignal,
};
pub use error::{MAX_RESPONSE_PREVIEW_CHARS, SandboxError, SandboxErrorCode};
pub use gateway::{
    DEFAULT_EXEC_TIMEOUT_SECONDS,
    ExecRequest,
    ExecResult,
    GatewayAuth,
    GatewayOptions,
    MAX_ERROR_BODY_BYTES,
    MAX_EXEC_TIMEOUT_SECONDS,
    MAX_TRANSFER_BYTES,
    UploadRequest,
    UploadResult,
};
pub use transport::{
    MAX_JSON_BODY_BYTES,
    ReqwestSandboxTransport,
    SandboxTransport,
    TransportRequest,
    TransportResponse,
};
pub use types::{
    DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_WAIT_POLL_INTERVAL,
    DEFAULT_WAIT_TIMEOUT,
    PRIME_SANDBOX_CREATE_MAX_ATTEMPTS,
    Sandbox,
    SandboxStatus,
    StartCommand,
    VmCreateRequest,
    WaitOptions,
};
pub use vm_error::{CommandSessionError, CommandSessionErrorCode};
pub use vm_process::{
    CommandSessionClient,
    CommandSessionOptions,
    ControlOptions,
    DEFAULT_EVENT_FRAME_MAX_BYTES,
    DEFAULT_KEEPALIVE_INTERVAL_SECONDS,
    DEFAULT_MAX_PENDING_EVENTS,
    DEFAULT_MAX_RECONNECTS,
    DEFAULT_RECONNECT_BASE_DELAY,
    DEFAULT_SEND_INPUT_TIMEOUT,
    DEFAULT_SEND_SIGNAL_TIMEOUT,
    DEFAULT_UNARY_ATTEMPTS,
    DEFAULT_UNARY_RETRY_BASE_DELAY,
    DEFAULT_UPDATE_TIMEOUT,
    GatewayAuthSource,
    MAX_PROCESS_INPUT_BYTES,
    MAX_UNARY_BODY_BYTES,
    PlatformGatewayAuthSource,
    SendInputOptions,
    SendSignalOptions,
    StartOptions,
    StreamOptions,
};
pub use vm_stream::CommandSessionStream;
