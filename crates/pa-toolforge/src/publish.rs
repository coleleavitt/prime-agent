//! Stage, gate, promote, install: the path by which the agent writes durable
//! capability.
//!
//! A refinement edit has no field that can carry source code, and both skill
//! screens require a module that already imports, so a refinement proposing
//! genuinely new code fails its own screen. Toolforge goes the other way
//! round: the source arrives first, is staged as a real package, and only
//! becomes a skill after it survives a gate it cannot grade itself.
//!
//! THE GATE IS A DOUBLE RUN. The exit test runs once against a stub whose
//! every attribute raises `NotImplementedError`, which MUST fail, and once
//! against the real staged package, which MUST pass. "Fails without, passes
//! with" is the whole claim a new capability makes, made executable. Only
//! then is the package promoted by rename into `<agentDir>/skills/<name>`,
//! where skill discovery finds it, the kernel bootstrap editable-installs it
//! and binds it in every later session.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pa_core::kernel::bootstrap::{
    PythonSkillPackageInstall,
    PythonSkillPackageInstallResult,
    install_python_skill_package,
    installed_kernel_python,
};

use crate::gate::{DEFAULT_GATE_TIMEOUT, OutcomeKind, run_exit_test};
use crate::ledger::{
    GatePhase,
    GateRun,
    LedgerRecord,
    PublishStatus,
    append_record,
    content_sha,
    ledger_path,
    load_ledger,
    next_version,
    toolforge_dir,
};
use crate::name::{js_len, validate_name};
use crate::package::{promote, src_path, stage_package, stage_stub};

/// Longest accepted package source, in UTF-16 units.
pub const MAX_SOURCE_CHARS: usize = 64_000;
/// Longest accepted exit test, in UTF-16 units.
pub const MAX_EXIT_TEST_CHARS: usize = 16_000;

/// What the agent asked to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRequest {
    pub name: String,
    /// Body of `src/<import>/__init__.py`; must define a callable `run`.
    pub source: String,
    /// One-paragraph description; becomes the SKILL.md description.
    pub doc: String,
    /// Program that must fail against a stub and pass against the package.
    pub exit_test: String,
}

/// The step a promotion runs under the installer's lock.
pub type PromoteStep = Box<dyn FnOnce() -> anyhow::Result<()> + Send>;
/// The future an installer returns.
pub type InstallFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<PythonSkillPackageInstallResult>> + Send>>;
/// Promote and install one accepted package: run the promotion step, then
/// make the package importable. The default is the kernel-venv installer;
/// tests substitute one that never touches the shared venv.
pub type PackageInstaller =
    Arc<dyn Fn(PythonSkillPackageInstall, PromoteStep) -> InstallFuture + Send + Sync>;

/// The kernel-venv installer.
#[must_use]
pub fn kernel_venv_installer() -> PackageInstaller {
    Arc::new(|request, promote_step| {
        Box::pin(async move { install_python_skill_package(&request, promote_step).await })
    })
}

/// Where a publish writes and what it runs with.
#[derive(Clone)]
pub struct PublishOptions {
    /// Where accepted packages are promoted to (`<agentDir>/skills`).
    pub skills_dir: PathBuf,
    /// `<agentDir>/toolforge/ledger.json`.
    pub ledger_path: PathBuf,
    /// Scratch root for staged packages and stubs
    /// (`<agentDir>/toolforge/staging`).
    pub staging_dir: PathBuf,
    /// Interpreter for both gate runs; `None` resolves the kernel python on
    /// disk at publish time.
    pub python: Option<PathBuf>,
    /// Import names already bound in this kernel; a publish may not shadow one.
    pub loaded_import_names: Vec<String>,
    pub installer: PackageInstaller,
    pub timeout: Duration,
    pub session_id: Option<String>,
}

impl PublishOptions {
    /// The defaults for an agent directory.
    #[must_use]
    pub fn for_agent_dir(agent_dir: &Path) -> Self {
        Self {
            skills_dir: agent_dir.join("skills"),
            ledger_path: ledger_path(agent_dir),
            staging_dir: toolforge_dir(agent_dir).join("staging"),
            python: None,
            loaded_import_names: Vec::new(),
            installer: kernel_venv_installer(),
            timeout: DEFAULT_GATE_TIMEOUT,
            session_id: None,
        }
    }
}

/// Which step refused a publish (the telemetry category; never content).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionStage {
    /// The name failed validation; nothing was written.
    Name,
    /// The source, doc or exit test was empty or too long.
    Shape,
    /// The exit test did not fail against the stub.
    Negative,
    /// The exit test did not pass against the package.
    Positive,
    /// Staging, promotion or the installer's own step failed.
    Error,
}

impl RejectionStage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Shape => "shape",
            Self::Negative => "negative",
            Self::Positive => "positive",
            Self::Error => "error",
        }
    }
}

/// How a publish ended. A rejection is a result with a reason, never an
/// error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishResult {
    pub status: PublishStatus,
    pub name: String,
    pub import_name: String,
    pub package_path: String,
    pub src_path: String,
    pub version: u64,
    pub installed: bool,
    pub gate: Vec<GateRun>,
    pub reason: Option<String>,
    pub install_detail: Option<String>,
    pub rejection: Option<RejectionStage>,
}

impl PublishResult {
    /// The host-request response the runtime's `rlm.toolforge.publish` reads.
    #[must_use]
    pub fn to_response(&self) -> serde_json::Value {
        let mut response = serde_json::json!({
            "status": self.status.as_str(),
            "name": self.name,
            "import_name": self.import_name,
            "package_path": self.package_path,
            "src_path": self.src_path,
            "version": self.version,
            "installed": self.installed,
            "gate": self.gate.iter().map(|run| serde_json::json!({
                "phase": run.phase.as_str(),
                "outcome": run.outcome,
                "detail": run.detail,
                "duration_ms": run.duration_ms,
                "ok": run.ok,
            })).collect::<Vec<_>>(),
        });
        if let Some(reason) = &self.reason {
            response["reason"] = reason.clone().into();
        }
        if let Some(detail) = &self.install_detail {
            response["install_detail"] = detail.clone().into();
        }
        response
    }
}

fn non_empty(text: &str) -> bool {
    !text.trim().is_empty()
}

fn shape_error(request: &PublishRequest) -> Option<String> {
    if !non_empty(&request.source) {
        return Some("toolforge source must be a non-empty string".to_string());
    }
    if !non_empty(&request.exit_test) {
        return Some("toolforge exit_test must be a non-empty string".to_string());
    }
    if !non_empty(&request.doc) {
        return Some("toolforge doc must be a non-empty string".to_string());
    }
    let source_length = js_len(&request.source);
    if source_length > MAX_SOURCE_CHARS {
        return Some(format!(
            "toolforge source exceeds {MAX_SOURCE_CHARS} characters ({source_length})"
        ));
    }
    let exit_test_length = js_len(&request.exit_test);
    if exit_test_length > MAX_EXIT_TEST_CHARS {
        return Some(format!(
            "toolforge exit_test exceeds {MAX_EXIT_TEST_CHARS} characters ({exit_test_length})"
        ));
    }
    None
}

/// Run both halves; the positive run only when the negative one held.
#[tracing::instrument(
    name = "toolforge.gate",
    skip_all,
    fields(
        toolforge.name = %name,
        toolforge.negative = tracing::field::Empty,
        toolforge.positive = tracing::field::Empty,
        toolforge.passed = tracing::field::Empty,
    )
)]
async fn run_gate(
    name: &str,
    exit_test: &str,
    stub_src: &Path,
    staged_src: &Path,
    python: Option<&Path>,
    timeout: Duration,
) -> Vec<GateRun> {
    let mut runs = Vec::with_capacity(2);
    for (phase, src) in [
        (GatePhase::Negative, stub_src),
        (GatePhase::Positive, staged_src),
    ] {
        let started = Instant::now();
        let cwd = src.parent().unwrap_or(src);
        let outcome = run_exit_test(exit_test, src, cwd, python, timeout).await;
        let ok = match phase {
            GatePhase::Negative => outcome.kind == OutcomeKind::Raised,
            GatePhase::Positive => outcome.kind == OutcomeKind::Clean,
        };
        runs.push(GateRun {
            phase,
            outcome: outcome.kind.as_str().to_string(),
            detail: outcome.detail,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            ok,
        });
        if !ok {
            break;
        }
    }
    let span = tracing::Span::current();
    span.record("toolforge.negative", runs[0].outcome.as_str());
    span.record(
        "toolforge.positive",
        runs.get(1).map_or("skipped", |run| run.outcome.as_str()),
    );
    span.record(
        "toolforge.passed",
        runs.len() == 2 && runs.iter().all(|run| run.ok),
    );
    runs
}

fn gate_rejection(gate: &[GateRun]) -> Option<(RejectionStage, String)> {
    for run in gate {
        if run.ok {
            continue;
        }
        return Some(match run.phase {
            GatePhase::Negative => {
                let did = if run.outcome == "clean" {
                    "passed"
                } else {
                    run.outcome.as_str()
                };
                (
                    RejectionStage::Negative,
                    format!(
                        "negative run did not fail: the exit test must raise against a stub that implements nothing, but it {did}. {}",
                        run.detail
                    ),
                )
            }
            GatePhase::Positive => (
                RejectionStage::Positive,
                format!(
                    "positive run did not pass: the exit test must succeed against the real package, but it {}. {}",
                    run.outcome, run.detail
                ),
            ),
        });
    }
    (gate.len() != 2).then(|| {
        (
            RejectionStage::Error,
            "gate did not complete both runs".to_string(),
        )
    })
}

/// The ledger record of one attempt.
fn record_of(
    request: &PublishRequest,
    import_name: &str,
    package_path: &str,
    outcome: &PublishResult,
    options: &PublishOptions,
) -> LedgerRecord {
    LedgerRecord {
        name: request.name.clone(),
        import_name: import_name.to_string(),
        package_path: package_path.to_string(),
        source_sha: content_sha(&request.source),
        exit_test_sha: content_sha(&request.exit_test),
        status: outcome.status,
        reason: outcome.reason.clone(),
        gate: outcome.gate.clone(),
        installed: outcome.installed,
        session_id: options.session_id.clone(),
        at: pa_core::session::manager::format_iso_now(),
        version: outcome.version,
    }
}

fn append_logged(record: LedgerRecord, path: &Path) {
    if let Err(error) = append_record(record, path) {
        tracing::warn!(path = %path.display(), error = %error, "toolforge.ledger.write_failed");
    }
}

/// One publish attempt past validation: what it stages where.
struct Attempt<'a> {
    request: &'a PublishRequest,
    import_name: &'a str,
    target: &'a Path,
    staged: &'a Path,
    stub: &'a Path,
    python: Option<&'a Path>,
    version: u64,
}

/// Stage both packages, run the gate, and promote + install an accepted
/// package, filling `result` in. An error is a failed step (staging,
/// promotion, the installer's own step), never a gate refusal.
async fn stage_gate_install(
    attempt: &Attempt<'_>,
    options: &PublishOptions,
    result: &mut PublishResult,
) -> anyhow::Result<()> {
    let request = attempt.request;
    let staged_src = stage_package(
        attempt.staged,
        &request.name,
        attempt.import_name,
        &request.source,
        &request.doc,
        &request.exit_test,
    )?;
    let stub_src = stage_stub(attempt.stub, attempt.import_name)?;
    result.gate = run_gate(
        &request.name,
        &request.exit_test,
        &stub_src,
        &staged_src,
        attempt.python,
        options.timeout,
    )
    .await;
    if let Some((stage, reason)) = gate_rejection(&result.gate) {
        result.rejection = Some(stage);
        result.reason = Some(reason);
        return Ok(());
    }
    let promote_from = attempt.staged.to_path_buf();
    let promote_to = attempt.target.to_path_buf();
    let installed = (options.installer)(
        PythonSkillPackageInstall {
            package_path: attempt.target.to_path_buf(),
            import_name: attempt.import_name.to_string(),
        },
        Box::new(move || Ok(promote(&promote_from, &promote_to)?)),
    )
    .await?;
    result.installed = installed.installed;
    result.install_detail = Some(installed.detail).filter(|detail| !detail.is_empty());
    result.status = PublishStatus::Published;
    result.version = attempt.version;
    Ok(())
}

/// Stage, gate, promote, install. Never fails for a rejection: a refusal is
/// a result with a reason, recorded in the ledger exactly like an acceptance,
/// so a failed capability attempt survives the session that made it.
#[tracing::instrument(
    name = "toolforge.publish",
    skip_all,
    fields(
        toolforge.name = %request.name,
        toolforge.import = tracing::field::Empty,
        toolforge.status = tracing::field::Empty,
        toolforge.reason = tracing::field::Empty,
        toolforge.installed = tracing::field::Empty,
        toolforge.gate_runs = tracing::field::Empty,
    )
)]
pub async fn publish(request: &PublishRequest, options: &PublishOptions) -> PublishResult {
    let span = tracing::Span::current();
    let import_name = match validate_name(&request.name, &options.loaded_import_names) {
        Ok(import_name) => import_name,
        Err(reason) => {
            span.record("toolforge.status", "rejected");
            span.record("toolforge.reason", "name");
            return PublishResult {
                status: PublishStatus::Rejected,
                name: request.name.clone(),
                import_name: String::new(),
                package_path: String::new(),
                src_path: String::new(),
                version: 0,
                installed: false,
                gate: Vec::new(),
                reason: Some(reason),
                install_detail: None,
                rejection: Some(RejectionStage::Name),
            };
        }
    };
    span.record("toolforge.import", import_name.as_str());
    let target = options.skills_dir.join(request.name.trim());
    let package_path = target.display().to_string();
    let target_src = src_path(&target).display().to_string();
    let mut result = PublishResult {
        status: PublishStatus::Rejected,
        name: request.name.clone(),
        import_name: import_name.clone(),
        package_path: package_path.clone(),
        src_path: target_src,
        version: 0,
        installed: false,
        gate: Vec::new(),
        reason: None,
        install_detail: None,
        rejection: None,
    };

    if let Some(reason) = shape_error(request) {
        span.record("toolforge.status", "rejected");
        span.record("toolforge.reason", "shape");
        result.reason = Some(reason);
        result.rejection = Some(RejectionStage::Shape);
        append_logged(
            record_of(request, &import_name, &package_path, &result, options),
            &options.ledger_path,
        );
        return result;
    }

    let attempt = uuid::Uuid::new_v4();
    let staged = options.staging_dir.join(format!("{import_name}-{attempt}"));
    let stub = options
        .staging_dir
        .join(format!("{import_name}-{attempt}.stub"));
    let python = options.python.clone().or_else(installed_kernel_python);
    let version = next_version(&load_ledger(&options.ledger_path), &request.name);

    let attempt_outcome = stage_gate_install(
        &Attempt {
            request,
            import_name: &import_name,
            target: &target,
            staged: &staged,
            stub: &stub,
            python: python.as_deref(),
            version,
        },
        options,
        &mut result,
    )
    .await;
    if let Err(error) = attempt_outcome {
        result.rejection = Some(RejectionStage::Error);
        result.reason = Some(format!("toolforge publish failed: {error:#}"));
    }
    let _ = std::fs::remove_dir_all(&staged);
    let _ = std::fs::remove_dir_all(&stub);

    append_logged(
        record_of(request, &import_name, &package_path, &result, options),
        &options.ledger_path,
    );
    span.record("toolforge.status", result.status.as_str());
    span.record("toolforge.installed", result.installed);
    span.record("toolforge.gate_runs", result.gate.len());
    if let Some(stage) = result.rejection {
        span.record("toolforge.reason", stage.as_str());
    }
    result
}
