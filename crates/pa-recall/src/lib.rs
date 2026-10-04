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
    is_build_claim_command, mentions_build_command, merge_recall_claims, ClaimStatus, ClaimVerdict,
    RecallClaim, RECALL_MAX_CLAIMS, RECALL_MAX_CLAIM_COMMAND_CHARS,
};
pub use feature::{
    is_workspace_recall_enabled, MarkOutcome, MarkWriter, RecallOptions, SkipReason,
    WorkspaceRecall, WORKSPACE_RECALL_ENV,
};
pub use git::{find_recall_repo, resolve_repo_root, GitFailure};
pub use mark::{
    capture_workspace, is_fully_verifiable, workspace_digest, AbsentPresence, AbsentSkipWorktree,
    CaptureFailure, WorkspaceSnapshot, WorkspaceState, RECALL_DIGEST_ALGORITHM,
    RECALL_UNVERIFIABLE,
};
pub use render::{render_recall_block, RECALL_BLOCK_MAX_BYTES};
pub use store::{
    read_recall_mark, read_recall_skip, recall_mark_path, recall_repo_key, recall_skip_path,
    write_recall_mark, MarkSkipReason, RecallClaimInput, RecallMarkFile, WrittenMark,
};
pub use time::format_iso;
pub use witness::{
    witness_workspace, RecallWitnessReport, RECALL_COMMITS_UNLISTED, RECALL_MARK_PREDATES_PRESENCE,
    RECALL_PRESENCE_CHANGED,
};
