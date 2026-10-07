//! The read-only `git rev-parse` probe: the current branch and its upstream.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use crate::context::GuardContext;
use crate::probe::{run_probe, ProbeLimits, ProbeOutcome};

/// The probe runs inside `bash()`, synchronously, so this is also the
/// session's worst-case freeze: a local rev-parse finishes in tens of
/// milliseconds, and a probe that cannot answer in time fails closed.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// One probe answers both questions: the upstream (implicit refspecs) and
/// the current branch (`HEAD` refspecs). An empty first line means the branch
/// has no upstream, which is not "not a repository".
const UPSTREAM_PROBE: &str = r#"cur=$(git rev-parse --abbrev-ref HEAD 2>/dev/null) || exit 1
up=$(git rev-parse --abbrev-ref --symbolic-full-name '@{u}' 2>/dev/null) || up=
printf '%s\n%s\n' "$up" "$cur"
"#;

/// The current branch and its upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UpstreamInfo {
    /// e.g. `origin/main`; `None` when the branch has none.
    pub upstream_ref: Option<String>,
    /// e.g. `feature`, or `HEAD` when detached.
    pub current_branch: String,
}

/// The probe did not answer inside [`PROBE_TIMEOUT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProbeTimedOut;

/// Probes already run during one check, by directory.
pub(super) type ProbeCache = HashMap<String, Option<UpstreamInfo>>;

/// Probe the branch and upstream in `cwd`: `None` when the probe cannot run
/// or `cwd` is not a repository (git itself then fails an implicit push).
/// The probe gets the shell and environment the guarded command would.
pub(super) fn probe_upstream(
    context: &GuardContext,
    cwd: &str,
    cache: &mut ProbeCache,
    timeout: Duration,
) -> Result<Option<UpstreamInfo>, ProbeTimedOut> {
    if let Some(info) = cache.get(cwd) {
        return Ok(info.clone());
    }
    let limits = ProbeLimits {
        timeout,
        kill_grace: Duration::from_secs(1),
        output_cap: None,
    };
    let info = match run_probe(context, UPSTREAM_PROBE, Path::new(cwd), limits) {
        ProbeOutcome::TimedOut => return Err(ProbeTimedOut),
        ProbeOutcome::Unavailable => None,
        ProbeOutcome::Finished { status, stdout, .. } => {
            let text = String::from_utf8_lossy(&stdout);
            let mut lines = text.split('\n');
            let upstream = lines.next().unwrap_or_default();
            let current = lines.next().unwrap_or_default();
            (status == Some(0) && !current.is_empty()).then(|| UpstreamInfo {
                upstream_ref: (!upstream.is_empty()).then(|| upstream.to_string()),
                current_branch: current.to_string(),
            })
        }
    };
    cache.insert(cwd.to_string(), info.clone());
    Ok(info)
}
