//! Workspace Recall (see `README.md`): per-repo marks of workspace digests
//! and build claims, written when a top-level session's agent run ends, and
//! a bounded `<workspace_recall>` block appended to the session's first
//! `ipython` result saying what changed since the last mark.
//!
//! The crate plugs into sessions only through
//! [`pa_core::features::SessionFeature`]; `pa-cli` installs
//! [`WorkspaceRecall`] behind its `recall` Cargo feature.

mod claims;
mod feature;
mod git;
mod mark;
mod render;
mod store;
mod time;
mod witness;

pub use claims::{
    ClaimStatus,
    ClaimVerdict,
    RECALL_MAX_CLAIM_COMMAND_CHARS,
    RECALL_MAX_CLAIMS,
    RecallClaim,
    is_build_claim_command,
    mentions_build_command,
    merge_recall_claims,
};
pub use feature::{
    MarkOutcome,
    MarkWriter,
    RecallOptions,
    SkipReason,
    WORKSPACE_RECALL_ENV,
    WorkspaceRecall,
    is_workspace_recall_enabled,
};
pub use git::{GitFailure, find_recall_repo, resolve_repo_root};
pub use mark::{
    AbsentPresence,
    AbsentSkipWorktree,
    CaptureFailure,
    RECALL_DIGEST_ALGORITHM,
    RECALL_UNVERIFIABLE,
    WorkspaceSnapshot,
    WorkspaceState,
    capture_workspace,
    is_fully_verifiable,
    workspace_digest,
};
pub use render::{RECALL_BLOCK_MAX_BYTES, render_recall_block};
pub use store::{
    MarkSkipReason,
    RecallClaimInput,
    RecallMarkFile,
    WrittenMark,
    read_recall_mark,
    read_recall_skip,
    recall_mark_path,
    recall_repo_key,
    recall_skip_path,
    write_recall_mark,
};
pub use time::format_iso;
pub use witness::{
    RECALL_COMMITS_UNLISTED,
    RECALL_MARK_PREDATES_PRESENCE,
    RECALL_PRESENCE_CHANGED,
    RecallWitnessReport,
    witness_workspace,
};
