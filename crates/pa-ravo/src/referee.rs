//! The referee (TS `ravo/referee.ts` verdicts and `referee-runner.ts`
//! adjudication): a proposal's claim to have fixed a recorded failure is
//! upheld or refuted by re-running the failure's verified replay cases,
//! never by reading the claim. A replay speaks only to a claim a skill the
//! proposal writes can fix (one importing what a missing-module or
//! missing-distribution probe names); every other claim is
//! `not_applicable` and stands on the provisional window instead.

use std::future::Future;
use std::pin::Pin;

use pa_core::refinement::planner::RefinementEdit;
use pa_core::refinement::{RefinementAction, RefinementKind};
use pa_ledger::{FailureRecord, ReplayCase, ReplayProbe, replay_probe_of};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The criterion-id prefix of a referee opponent.
pub const REFEREE_OPPONENT_PREFIX: &str = "referee:";

/// How one replay run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// The case raised `exception_class`.
    Raised {
        exception_class: String,
        detail: String,
    },
    /// The case completed without raising.
    Clean { detail: String },
    /// The run itself could not be performed; never a pass.
    Unrunnable { detail: String },
}

impl ReplayOutcome {
    fn detail(&self) -> &str {
        match self {
            Self::Raised { detail, .. } | Self::Clean { detail } | Self::Unrunnable { detail } => {
                detail
            }
        }
    }
}

/// The environment a replay runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayEnvironment {
    /// `PATH`, `HOME`, `LANG` and explicit roots: for a probe derived from
    /// tool output (the capture-time self-check).
    Sanitized,
    /// The sanitized base plus the host's `PYTHONPATH` and the skill source
    /// roots: the environment the skill dry-run screen imports in.
    SkillImport,
}

/// Runs replay cases. Implementations never fail: every way a run can go
/// wrong is [`ReplayOutcome::Unrunnable`].
pub trait ReplayRunner: Send + Sync {
    /// Run `case` in `environment` with `sys_path` roots prepended.
    fn run<'a>(
        &'a self,
        case: &'a ReplayCase,
        environment: ReplayEnvironment,
        sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>>;
}

/// A referee verdict on one claimed fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefereeVerdictStatus {
    /// A verified case raised its recorded exception again: refuted.
    Upheld,
    /// Every verified applicable case ran clean: corroborated.
    Cleared,
    /// A verified case could not run, or raised something else.
    Unverifiable,
    /// Applicable cases exist but none ever reproduced: fails closed.
    NoEvidence,
    /// No replay can speak to the claim.
    NotApplicable,
}

/// One verdict. Field order is the TS object's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefereeVerdict {
    pub fingerprint_id: String,
    pub status: RefereeVerdictStatus,
    pub detail: String,
}

/// `referee:<fingerprint>` (idempotent).
#[must_use]
pub fn referee_opponent_id(fingerprint_id: &str) -> String {
    if fingerprint_id.starts_with(REFEREE_OPPONENT_PREFIX) {
        fingerprint_id.to_string()
    } else {
        format!("{REFEREE_OPPONENT_PREFIX}{fingerprint_id}")
    }
}

/// Whether a criterion id is a referee opponent.
#[must_use]
pub fn is_referee_opponent_id(criterion_id: &str) -> bool {
    criterion_id.len() > REFEREE_OPPONENT_PREFIX.len()
        && criterion_id.starts_with(REFEREE_OPPONENT_PREFIX)
}

/// The fingerprint a referee opponent names.
#[must_use]
pub fn referee_opponent_fingerprint(criterion_id: &str) -> Option<&str> {
    is_referee_opponent_id(criterion_id).then(|| &criterion_id[REFEREE_OPPONENT_PREFIX.len()..])
}

fn is_module_path(text: &str) -> bool {
    let mut segments = text.split('.');
    segments.all(|segment| {
        let mut chars = segment.chars();
        chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

/// The modules a proposal's skill creates and updates import: each python
/// reference's `import` (or legacy `python_import`), in first-seen order.
#[must_use]
pub fn skill_imports_of(edits: &[RefinementEdit]) -> Vec<String> {
    let mut imports: Vec<String> = Vec::new();
    for edit in edits {
        if edit.kind != Some(RefinementKind::Skill)
            || !matches!(
                edit.action,
                Some(RefinementAction::Create | RefinementAction::Update)
            )
        {
            continue;
        }
        let Some(reference) = &edit.reference else {
            continue;
        };
        if reference.get("type").and_then(Value::as_str) != Some("python") {
            continue;
        }
        let modern = reference
            .get("import")
            .and_then(Value::as_str)
            .filter(|text| !js_trim(text).is_empty());
        let raw = modern.or_else(|| reference.get("python_import").and_then(Value::as_str));
        let module = raw.map(js_trim).unwrap_or_default();
        if is_module_path(module) && !imports.iter().any(|seen| seen == module) {
            imports.push(module.to_string());
        }
    }
    imports
}

/// `String.prototype.trim`.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{FEFF}')
}

/// `text.split(/[-_.]+/)`.
fn split_separator_runs(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_run = false;
    for (index, ch) in text.char_indices() {
        let separator = matches!(ch, '-' | '_' | '.');
        if separator && !in_run {
            parts.push(&text[start..index]);
            in_run = true;
        } else if !separator && in_run {
            start = index;
            in_run = false;
        }
    }
    parts.push(if in_run { "" } else { &text[start..] });
    parts
}

fn module_paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|rest| rest.starts_with('.'))
        || right
            .strip_prefix(left)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// PEP 503 comparison: one name's segments a prefix of the other's.
fn distribution_overlaps_module(distribution: &str, module: &str) -> bool {
    let distribution = distribution.to_lowercase();
    let module = module.to_lowercase();
    split_separator_runs(&distribution)
        .iter()
        .zip(split_separator_runs(&module).iter())
        .all(|(left, right)| left == right)
}

/// Whether a case can speak to a proposal whose skills import
/// `skill_imports`.
#[must_use]
pub fn replay_applies_to_skill_imports(case: &ReplayCase, skill_imports: &[String]) -> bool {
    match replay_probe_of(&case.source) {
        Some(ReplayProbe::Module(module)) => skill_imports
            .iter()
            .any(|import| module_paths_overlap(&module, import)),
        Some(ReplayProbe::Distribution(distribution)) => skill_imports
            .iter()
            .any(|import| distribution_overlaps_module(&distribution, import)),
        None => false,
    }
}

fn bare_class(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// The verdict a case's run gives (Rocq Def 16.1): a function of the case
/// and its run alone.
#[must_use]
pub fn verdict_from_outcome(case: &ReplayCase, outcome: &ReplayOutcome) -> RefereeVerdictStatus {
    match outcome {
        ReplayOutcome::Unrunnable { .. } => RefereeVerdictStatus::Unverifiable,
        ReplayOutcome::Raised {
            exception_class, ..
        } => match &case.exception_class {
            Some(expected) if bare_class(expected) != bare_class(exception_class) => {
                RefereeVerdictStatus::Unverifiable
            }
            _ => RefereeVerdictStatus::Upheld,
        },
        ReplayOutcome::Clean { .. } => {
            if case.verified_at.is_some() {
                RefereeVerdictStatus::Cleared
            } else {
                RefereeVerdictStatus::NoEvidence
            }
        }
    }
}

/// Whether a verdict is executable evidence (joins the pool as
/// `referee:<fp>`).
#[must_use]
pub fn referee_verdict_is_evidence(verdict: Option<&RefereeVerdict>) -> bool {
    verdict.is_some_and(|verdict| {
        !matches!(
            verdict.status,
            RefereeVerdictStatus::NoEvidence | RefereeVerdictStatus::NotApplicable
        )
    })
}

/// `failure:<fp>` passes on the claim alone unless executable evidence
/// speaks; a derivable failure that never reproduced fails closed.
#[must_use]
pub fn failure_opponent_passed(claimed: bool, verdict: Option<&RefereeVerdict>) -> bool {
    if !claimed {
        return false;
    }
    match verdict {
        None => true,
        Some(verdict) => matches!(
            verdict.status,
            RefereeVerdictStatus::NotApplicable | RefereeVerdictStatus::Cleared
        ),
    }
}

/// `referee:<fp>` charges only a claim, and fails closed on anything but a
/// clear.
#[must_use]
pub fn referee_opponent_passed(claimed: bool, verdict: Option<&RefereeVerdict>) -> bool {
    if !claimed {
        return true;
    }
    failure_opponent_passed(claimed, verdict)
}

/// The detail a referee opponent's certificate line carries.
#[must_use]
pub fn referee_detail(verdict: Option<&RefereeVerdict>, claimed: bool) -> String {
    if !claimed {
        return "no claim to adjudicate".to_string();
    }
    verdict.map_or_else(
        || "no replay case recorded for this fingerprint".to_string(),
        |verdict| verdict.detail.clone(),
    )
}

fn verdict(fingerprint_id: &str, status: RefereeVerdictStatus, detail: String) -> RefereeVerdict {
    RefereeVerdict {
        fingerprint_id: fingerprint_id.to_string(),
        status,
        detail,
    }
}

fn status_name(status: RefereeVerdictStatus) -> &'static str {
    match status {
        RefereeVerdictStatus::Upheld => "upheld",
        RefereeVerdictStatus::Cleared => "cleared",
        RefereeVerdictStatus::Unverifiable => "unverifiable",
        RefereeVerdictStatus::NoEvidence => "no_evidence",
        RefereeVerdictStatus::NotApplicable => "not_applicable",
    }
}

async fn adjudicate_record(
    record: &FailureRecord,
    skill_imports: &[String],
    sys_path: &[String],
    runner: &dyn ReplayRunner,
) -> RefereeVerdict {
    let fingerprint_id = record.fingerprint.id.as_str();
    let cases: Vec<&ReplayCase> = record
        .replay_cases
        .iter()
        .filter(|case| replay_probe_of(&case.source).is_some())
        .collect();
    let applicable: Vec<&ReplayCase> = cases
        .iter()
        .copied()
        .filter(|case| replay_applies_to_skill_imports(case, skill_imports))
        .collect();
    if applicable.is_empty() {
        let detail = if cases.is_empty() {
            "not_applicable: no replay case derivable for this failure"
        } else {
            "not_applicable: no skill the proposal writes imports what this failure's replay cases probe"
        };
        return verdict(
            fingerprint_id,
            RefereeVerdictStatus::NotApplicable,
            detail.to_string(),
        );
    }
    let verified: Vec<&ReplayCase> = applicable
        .into_iter()
        .filter(|case| case.verified_at.is_some())
        .collect();
    if verified.is_empty() {
        return verdict(
            fingerprint_id,
            RefereeVerdictStatus::NoEvidence,
            "no_evidence: no replay case probing an import of the proposal's skills ever reproduced this failure"
                .to_string(),
        );
    }
    let mut results: Vec<(RefereeVerdictStatus, String)> = Vec::new();
    for case in &verified {
        let outcome = runner
            .run(case, ReplayEnvironment::SkillImport, sys_path)
            .await;
        results.push((
            verdict_from_outcome(case, &outcome),
            outcome.detail().to_string(),
        ));
    }
    let status = if results
        .iter()
        .any(|(status, _)| *status == RefereeVerdictStatus::Upheld)
    {
        RefereeVerdictStatus::Upheld
    } else if results
        .iter()
        .all(|(status, _)| *status == RefereeVerdictStatus::Cleared)
    {
        RefereeVerdictStatus::Cleared
    } else {
        RefereeVerdictStatus::Unverifiable
    };
    let decisive = results
        .iter()
        .find(|(result, _)| *result == status)
        .unwrap_or(&results[0]);
    let suffix = if verified.len() > 1 {
        format!(" ({} verified cases)", verified.len())
    } else {
        String::new()
    };
    verdict(
        fingerprint_id,
        status,
        format!("{}: {}{suffix}", status_name(status), decisive.1),
    )
}

/// Adjudicate the fingerprints a proposal claims, one record at a time, in
/// record order. Records it does not claim are not adjudicated.
pub async fn adjudicate_failure_claims(
    records: &[FailureRecord],
    claimed_fingerprint_ids: &[String],
    skill_imports: &[String],
    sys_path: &[String],
    runner: &dyn ReplayRunner,
) -> Vec<RefereeVerdict> {
    let mut verdicts = Vec::new();
    for record in records {
        if !claimed_fingerprint_ids.contains(&record.fingerprint.id) {
            continue;
        }
        verdicts.push(adjudicate_record(record, skill_imports, sys_path, runner).await);
    }
    verdicts
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use pa_core::refinement::planner::normalize_refinement_proposal;
    use pa_ledger::{FailureKind, fingerprint_failure};
    use serde_json::json;

    use super::*;

    /// Answers each run from a script, in order, and records what ran.
    struct Scripted {
        outcomes: Mutex<Vec<ReplayOutcome>>,
        ran: Mutex<Vec<(String, ReplayEnvironment, Vec<String>)>>,
    }

    impl ReplayRunner for Scripted {
        fn run<'a>(
            &'a self,
            case: &'a ReplayCase,
            environment: ReplayEnvironment,
            sys_path: &'a [String],
        ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
            self.ran
                .lock()
                .unwrap()
                .push((case.source.clone(), environment, sys_path.to_vec()));
            let outcome = self.outcomes.lock().unwrap().remove(0);
            Box::pin(async move { outcome })
        }
    }

    fn case(source: &str, verified: bool) -> ReplayCase {
        ReplayCase {
            language: "python".to_string(),
            source: source.to_string(),
            exception_class: Some("ModuleNotFoundError".to_string()),
            sys_path: None,
            verified_at: verified.then(|| "2026-01-01T00:00:00.000Z".to_string()),
        }
    }

    fn record(id_seed: &str, cases: Vec<ReplayCase>) -> FailureRecord {
        FailureRecord {
            fingerprint: fingerprint_failure(
                FailureKind::PythonException,
                Some("ipython"),
                Some("ModuleNotFoundError"),
                id_seed,
            ),
            count: 2,
            first_seen_turn: 1,
            last_seen_turn: 2,
            first_seen_at: String::new(),
            last_seen_at: String::new(),
            excerpt: String::new(),
            addressed_by_proposal_ids: Vec::new(),
            replay_cases: cases,
            non_actionable_count: None,
        }
    }

    fn clean() -> ReplayOutcome {
        ReplayOutcome::Clean {
            detail: "clean".to_string(),
        }
    }

    fn raised(class: &str) -> ReplayOutcome {
        ReplayOutcome::Raised {
            exception_class: class.to_string(),
            detail: format!("{class}: boom"),
        }
    }

    #[tokio::test]
    async fn verdicts_aggregate_like_the_ts_referee() {
        let upheld = record(
            "a",
            vec![case("import foo", true), case("import foo.bar", true)],
        );
        let cleared = record("b", vec![case("import foo", true)]);
        let mixed = record(
            "c",
            vec![case("import foo", true), case("import foo.baz", true)],
        );
        let missing = record("d", vec![case("import foo", false)]);
        let elsewhere = record("e", vec![case("import other", true)]);
        let none = record("f", Vec::new());
        let unclaimed = record("g", vec![case("import foo", true)]);
        let runner = Scripted {
            outcomes: Mutex::new(vec![
                clean(),
                raised("ModuleNotFoundError"),
                clean(),
                raised("ImportError"),
                clean(),
            ]),
            ran: Mutex::new(Vec::new()),
        };
        let records = vec![
            upheld.clone(),
            cleared.clone(),
            mixed.clone(),
            missing.clone(),
            elsewhere.clone(),
            none.clone(),
            unclaimed,
        ];
        let claimed: Vec<String> = [&upheld, &cleared, &mixed, &missing, &elsewhere, &none]
            .iter()
            .map(|record| record.fingerprint.id.clone())
            .collect();
        let verdicts = adjudicate_failure_claims(
            &records,
            &claimed,
            &["foo.bar".to_string(), "foo.baz".to_string()],
            &["/roots".to_string()],
            &runner,
        )
        .await;
        let expected = |record: &FailureRecord, status, detail: &str| RefereeVerdict {
            fingerprint_id: record.fingerprint.id.clone(),
            status,
            detail: detail.to_string(),
        };
        assert_eq!(
            verdicts,
            [
                expected(
                    &upheld,
                    RefereeVerdictStatus::Upheld,
                    "upheld: ModuleNotFoundError: boom (2 verified cases)"
                ),
                expected(&cleared, RefereeVerdictStatus::Cleared, "cleared: clean"),
                expected(
                    &mixed,
                    RefereeVerdictStatus::Unverifiable,
                    "unverifiable: ImportError: boom (2 verified cases)"
                ),
                expected(
                    &missing,
                    RefereeVerdictStatus::NoEvidence,
                    "no_evidence: no replay case probing an import of the proposal's skills ever reproduced this failure"
                ),
                expected(
                    &elsewhere,
                    RefereeVerdictStatus::NotApplicable,
                    "not_applicable: no skill the proposal writes imports what this failure's replay cases probe"
                ),
                expected(
                    &none,
                    RefereeVerdictStatus::NotApplicable,
                    "not_applicable: no replay case derivable for this failure"
                ),
            ]
        );
        let ran: Vec<(String, ReplayEnvironment, Vec<String>)> = runner.ran.into_inner().unwrap();
        assert_eq!(
            ran.iter()
                .map(|(source, _, _)| source.as_str())
                .collect::<Vec<_>>(),
            [
                "import foo",
                "import foo.bar",
                "import foo",
                "import foo",
                "import foo.baz"
            ]
        );
        assert!(ran.iter().all(|(_, environment, roots)| {
            *environment == ReplayEnvironment::SkillImport && roots == &["/roots".to_string()]
        }));
    }

    #[test]
    fn skill_imports_and_distribution_overlap_follow_the_ts_rules() {
        let proposal = normalize_refinement_proposal(&json!({ "edits": [
            { "action": "create", "kind": "skill", "reference": { "type": "python", "import": " pkg.mod " } },
            { "action": "update", "kind": "skill", "reference": { "type": "python", "import": "  ", "python_import": "legacy" } },
            { "action": "delete", "kind": "skill", "reference": { "type": "python", "import": "gone" } },
            { "action": "create", "kind": "memory", "reference": { "type": "python", "import": "nope" } },
            { "action": "create", "kind": "skill", "reference": { "type": "shell", "import": "nope" } },
            { "action": "create", "kind": "skill", "reference": { "type": "python", "import": "bad-name" } },
            { "action": "create", "kind": "skill", "reference": { "type": "python", "import": "pkg.mod" } }
        ] }));
        assert_eq!(skill_imports_of(&proposal.edits), ["pkg.mod", "legacy"]);
        let probe = |source: &str| ReplayCase {
            language: "python".to_string(),
            source: source.to_string(),
            exception_class: None,
            sys_path: None,
            verified_at: None,
        };
        let distribution =
            probe("import importlib.metadata\nimportlib.metadata.version(\"Pkg-Mod\")");
        assert!(replay_applies_to_skill_imports(
            &distribution,
            &["pkg.mod.inner".to_string()]
        ));
        assert!(!replay_applies_to_skill_imports(
            &distribution,
            &["pkgx".to_string()]
        ));
        assert!(replay_applies_to_skill_imports(
            &probe("import pkg"),
            &["pkg.mod".to_string()]
        ));
        assert!(!replay_applies_to_skill_imports(
            &probe("import pk"),
            &["pkg.mod".to_string()]
        ));
        assert!(!replay_applies_to_skill_imports(
            &probe("import os; os.system('x')"),
            &["os".to_string()]
        ));
    }
}
