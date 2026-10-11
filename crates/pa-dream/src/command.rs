//! `prime-agent dream …`: argv parsing and printing (TS `cli/dream-command.ts`).
//!
//! Everything here is parsing and output; the framework is the rest of the
//! crate. The standalone runner has no agent handler, so `--llm-proposer`,
//! `--llm-dreamer` and the guided experiment arms are rejected before anything
//! runs: no token is spent and no socket is opened.

use std::path::{Path, PathBuf};

use pa_types::js::js_trim;
use serde_json::{Map, Value};

use crate::dream_loop::{DreamLoopOptions, DreamLoopResult, run_dream_loop};
use crate::dreams::DreamsLogContext;
use crate::experiment::{
    EXPERIMENT_ARMS,
    ExperimentArm,
    ExperimentArmResult,
    ExperimentBudget,
    ExperimentError,
    ExperimentResult,
    ExperimentRoundRow,
    ExperimentRunOptions,
    ExperimentSpec,
    GUIDED_ARM_REJECTION_MESSAGE,
    LOCAL_EXPERIMENT_ARMS,
    run_experiment,
};
use crate::improve::{CandidateReason, DreamingOptions, run_dreaming};
use crate::json::{self, js_number, to_fixed};
use crate::objective::{
    DEFAULT_OBJECTIVE,
    ObjectiveBudget,
    ReplayObjectiveConfig,
    compute_objective,
    pool_score_scale,
};
use crate::policy::{DEFAULT_POLICY, ExplorationPolicy, PRIMING_DIVERSE, policy_id};
use crate::replay::simulate_policy_with_span;
use crate::rng::{Seed, SeededRng};
use crate::rollout::{ExploreOptions, run_online_exploration};
use crate::store::{
    DreamStoreError,
    RecordedTree,
    TreeSummary,
    dream_dir,
    list_experiment_ids,
    list_trees,
    read_tree,
};
use crate::tasks::{DREAM_TASK_IDS, DreamTaskId, resolve_task, resolve_task_n};

/// The one usage string (`help dream` prints it).
pub const DREAM_USAGE: &str = "dream [rollout|replay|improve|loop|experiment|status|show] [--task <circle-packing|sum-difference|python-speedup|autocorrelation>] [--n <size>] [--seed <n>] [--seeds <a,b,c>] [--workers <n>] [--k1 <n>] [--k2 <n>] [--dreams <n>] [--beta1 <x>] [--beta2 <x>] [--beta3 <x>] [--iterations <n>] [--rounds <n>] [--arms <dream,fixed>] [--priming <none|diverse>] [--overwrite] [--tree <id>] [--dir <path>] [--llm-proposer] [--llm-dreamer] [--json]";

/// The one-line summary `help` lists.
pub const DREAM_SUMMARY: &str = "Run the Dream-RSI explore/replay/improve loop, or its controlled experiment, on a local scored task";

/// The `help dream` description.
pub const DREAM_DESCRIPTION: &str = "Grows a discovery tree with a fixed, serializable exploration policy, freezes each tree into a zero-cost replay simulator, and improves the policy by local search over its typed parameters. The default subcommand is loop and the default task is circle-packing (n=26). experiment (alias compare) runs the paper's controlled comparison: every arm starts from the same policy, seed and per-round budget, and the fixed arm (Recursive Fixed Exploration) never dreams, so on a deterministically scored task round 1 is identical across arms by construction; python-speedup is wall-clock scored, so its round-1 scores differ within timing noise, and result.json records scoring as deterministic or timing. Per-round rows, the headline multipliers and a versioned result.json land under <dir>/experiments/<id>/ for evals/dream/plot_experiment.py; per arm, final policy is the last one deployed and selected policy is the post-hoc winner on the arm's own pool. The local proposer and local policy search spend no model tokens and use no network. --llm-proposer, --llm-dreamer and the dream-guided/fixed-guided arms require an in-session agent handler and are rejected by the standalone CLI.";

/// The `help dream` option rows: one per flag `DREAM_USAGE` names.
pub const DREAM_OPTIONS: &[&str] = &[
    "--task <name>     Scored task, one of circle-packing, sum-difference, python-speedup, autocorrelation (default: circle-packing)",
    "--n <26|32>       Circle count for circle-packing (default: 26)",
    "--seed <n>        Seed for the injected RNG (default: 1)",
    "--seeds <a,b,c>   experiment: run one experiment per seed, sequentially (overrides --seed)",
    "--workers <n>     Max parallelism W, cells per round (default: 4)",
    "--k1 <n>          Max online exploration rounds (default: 12)",
    "--k2 <n>          Max replay rounds per policy simulation (default: 24)",
    "--dreams <n>      Revised policies M per dreaming step (default: 16)",
    "--beta1 <x>       Replay objective: cost of spending the whole W*k1 probe budget, in quality points (default: 0.05)",
    "--beta2 <x>       Replay objective: bonus for finishing in zero rounds, scaled by 1 - rounds/k1 (default: 0.1)",
    "--beta3 <x>       Replay objective: weight of the anytime (best-so-far over the budget) term against final quality, in [0, 1] (default: 0.25)",
    "--iterations <n>  Explore/dream/redeploy iterations for loop (default: 3)",
    "--rounds <n>      experiment: rollouts per arm (default: 4)",
    "--arms <list>     experiment: comma-separated distinct arms out of dream, fixed, dream-guided, fixed-guided (default: dream,fixed)",
    "--priming <mode>  Pool priming at round 1: none (default) or diverse (explore-root/never/batch 8 and best-first/never/batch 1, charged to round 1)",
    "--overwrite       experiment: replace an existing result.json for the same id",
    "--tree <id>       Tree id for replay/show (default: latest)",
    "--dir <path>      Dream store directory (default: <agent-dir>/dream)",
    "--llm-proposer    Use the in-session LLM proposer (rejected by the CLI)",
    "--llm-dreamer     Use the in-session LLM policy improver (rejected by the CLI)",
    "--json            Print the result as JSON",
];

/// The `help dream` examples.
pub const DREAM_EXAMPLES: &[&str] = &[
    "dream",
    "dream rollout --task circle-packing --seed 7",
    "dream loop --seed 7 --json",
    "dream experiment --task python-speedup --rounds 5 --seeds 1,2,3 --json",
    "dream experiment --task circle-packing --rounds 4 --arms dream,fixed --dir /tmp/dream-evidence",
    "dream show --tree latest",
];

const LLM_REJECTION_MESSAGE: &str = "LLM proposer/dreamer run only in-session, where an agent handler exists: start them with /dream --llm-proposer or /dream --llm-dreamer. The standalone CLI has no handler, so it runs the default local proposer at zero tokens.";

/// A subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamSubcommand {
    Rollout,
    Replay,
    Improve,
    Loop,
    Experiment,
    Status,
    Show,
}

impl DreamSubcommand {
    /// The canonical name (the telemetry vocabulary).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rollout => "rollout",
            Self::Replay => "replay",
            Self::Improve => "improve",
            Self::Loop => "loop",
            Self::Experiment => "experiment",
            Self::Status => "status",
            Self::Show => "show",
        }
    }

    fn from_alias(text: &str) -> Option<Self> {
        Some(match text {
            "rollout" | "propose" => Self::Rollout,
            "replay" | "simulate" => Self::Replay,
            "improve" => Self::Improve,
            "loop" => Self::Loop,
            "experiment" | "compare" => Self::Experiment,
            "status" => Self::Status,
            "show" | "inspect" => Self::Show,
            _ => return None,
        })
    }
}

/// `--priming`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamPriming {
    None,
    Diverse,
}

/// Parsed `dream` options.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)] // one field per CLI switch, as the TS options object
pub struct DreamCommandOptions {
    pub subcommand: DreamSubcommand,
    pub task: DreamTaskId,
    pub n: Option<usize>,
    pub seed: u64,
    pub seeds: Option<Vec<u64>>,
    pub workers: u32,
    pub k1: u32,
    pub k2: u32,
    pub dreams: u32,
    pub objective: ReplayObjectiveConfig,
    pub iterations: u32,
    pub rounds: u32,
    pub arms: Vec<ExperimentArm>,
    pub priming: DreamPriming,
    pub overwrite: bool,
    pub tree: String,
    pub dir: Option<PathBuf>,
    pub json: bool,
    pub llm_proposer: bool,
    pub llm_dreamer: bool,
}

/// A usage error (exit 1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DreamCommandUsageError(pub String);

fn usage(message: impl Into<String>) -> DreamCommandUsageError {
    DreamCommandUsageError(message.into())
}

/// `Number.parseInt(raw, 10)` that must print back as `raw.trim()` (JS trim:
/// U+FEFF is white space, U+0085 is not) and be a safe integer.
fn integer(raw: &str) -> Option<u64> {
    let trimmed = js_trim(raw);
    let parsed: u64 = trimmed.parse().ok()?;
    (parsed.to_string() == trimmed && parsed < (1u64 << 53)).then_some(parsed)
}

fn positive_u32(raw: &str, option: &str) -> Result<u32, DreamCommandUsageError> {
    integer(raw)
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| usage(format!("{option} requires a positive integer.")))
}

fn non_negative_integer(raw: &str, option: &str) -> Result<u64, DreamCommandUsageError> {
    integer(raw).ok_or_else(|| usage(format!("{option} requires a non-negative integer.")))
}

/// `Number(raw.trim())`, except that a blank value is rejected rather than
/// read as `0`, and the result must be finite and not below zero (`-0` passes).
fn non_negative_number(raw: &str, option: &str) -> Result<f64, DreamCommandUsageError> {
    let trimmed = js_trim(raw);
    let parsed = if trimmed.is_empty() {
        f64::NAN
    } else {
        pa_types::js::js_number(trimmed)
    };
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err(usage(format!(
            "{option} requires a finite non-negative number."
        )))
    }
}

/// Parse `dream` argv.
///
/// # Errors
///
/// [`DreamCommandUsageError`] for a bad or unknown flag, value or subcommand.
#[allow(clippy::too_many_lines)] // one flag table, as in the TS
pub fn parse_dream_command_args(
    args: &[String],
) -> Result<DreamCommandOptions, DreamCommandUsageError> {
    let mut subcommand: Option<DreamSubcommand> = None;
    let mut options = DreamCommandOptions {
        subcommand: DreamSubcommand::Loop,
        task: DreamTaskId::CirclePacking,
        n: None,
        seed: 1,
        seeds: None,
        workers: 4,
        k1: 12,
        k2: 24,
        dreams: 16,
        objective: DEFAULT_OBJECTIVE,
        iterations: 3,
        rounds: 4,
        arms: LOCAL_EXPERIMENT_ARMS.to_vec(),
        priming: DreamPriming::None,
        overwrite: false,
        tree: "latest".to_string(),
        dir: None,
        json: false,
        llm_proposer: false,
        llm_dreamer: false,
    };
    let mut iterations_given = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (flag, inline) = match (arg.starts_with("--"), arg.find('=')) {
            (true, Some(eq)) => (&arg[..eq], Some(&arg[eq + 1..])),
            _ => (arg.as_str(), None),
        };
        let mut value = |option: &str| -> Result<String, DreamCommandUsageError> {
            if let Some(inline) = inline {
                if inline.is_empty() {
                    return Err(usage(format!("{option} requires a value.")));
                }
                return Ok(inline.to_string());
            }
            index += 1;
            match args.get(index) {
                Some(next) if !next.starts_with('-') => Ok(next.clone()),
                _ => Err(usage(format!("{option} requires a value."))),
            }
        };
        match flag {
            "--json" => options.json = true,
            "--llm-proposer" => options.llm_proposer = true,
            "--llm-dreamer" => options.llm_dreamer = true,
            "--task" => {
                let raw = value("--task")?;
                options.task = DreamTaskId::from_name(&raw).ok_or_else(|| {
                    let ids: Vec<&str> = DREAM_TASK_IDS.iter().map(|id| id.as_str()).collect();
                    usage(format!(
                        "Unknown task: {raw}. Use one of {}.",
                        ids.join(", ")
                    ))
                })?;
            }
            "--n" => {
                let raw = value("--n")?;
                options.n = Some(usize::try_from(positive_u32(&raw, "--n")?).unwrap_or(usize::MAX));
            }
            "--seed" => options.seed = non_negative_integer(&value("--seed")?, "--seed")?,
            "--workers" => options.workers = positive_u32(&value("--workers")?, "--workers")?,
            "--k1" => options.k1 = positive_u32(&value("--k1")?, "--k1")?,
            "--k2" => options.k2 = positive_u32(&value("--k2")?, "--k2")?,
            "--dreams" => options.dreams = positive_u32(&value("--dreams")?, "--dreams")?,
            "--beta1" => {
                options.objective.beta1 = non_negative_number(&value("--beta1")?, "--beta1")?;
            }
            "--beta2" => {
                options.objective.beta2 = non_negative_number(&value("--beta2")?, "--beta2")?;
            }
            "--beta3" => {
                options.objective.beta3 = non_negative_number(&value("--beta3")?, "--beta3")?;
                if options.objective.beta3 > 1.0 {
                    return Err(usage("--beta3 must be in [0, 1]."));
                }
            }
            "--priming" => {
                options.priming = match value("--priming")?.as_str() {
                    "none" => DreamPriming::None,
                    "diverse" => DreamPriming::Diverse,
                    _ => return Err(usage("--priming must be none or diverse.")),
                };
            }
            "--iterations" => {
                options.iterations = positive_u32(&value("--iterations")?, "--iterations")?;
                iterations_given = true;
            }
            "--rounds" => options.rounds = positive_u32(&value("--rounds")?, "--rounds")?,
            "--arms" => {
                let raw = value("--arms")?;
                let names: Vec<&str> = raw
                    .split(',')
                    .map(js_trim)
                    .filter(|name| !name.is_empty())
                    .collect();
                if names.is_empty() {
                    return Err(usage("--arms requires a comma-separated list of arms."));
                }
                let mut arms = Vec::new();
                for name in names {
                    let arm = ExperimentArm::from_name(name).ok_or_else(|| {
                        let known: Vec<&str> =
                            EXPERIMENT_ARMS.iter().map(|arm| arm.as_str()).collect();
                        usage(format!(
                            "Unknown arm: {name}. Use one of {}.",
                            known.join(", ")
                        ))
                    })?;
                    if arms.contains(&arm) {
                        return Err(usage(format!("--arms lists {name} twice.")));
                    }
                    arms.push(arm);
                }
                options.arms = arms;
            }
            "--seeds" => {
                let raw = value("--seeds")?;
                let parts: Vec<&str> = raw
                    .split(',')
                    .map(js_trim)
                    .filter(|part| !part.is_empty())
                    .collect();
                if parts.is_empty() {
                    return Err(usage("--seeds requires a comma-separated list of seeds."));
                }
                let seeds = parts
                    .iter()
                    .map(|part| non_negative_integer(part, "--seeds"))
                    .collect::<Result<Vec<u64>, _>>()?;
                let distinct: std::collections::HashSet<u64> = seeds.iter().copied().collect();
                if distinct.len() != seeds.len() {
                    return Err(usage("--seeds must be distinct."));
                }
                options.seeds = Some(seeds);
            }
            "--overwrite" => options.overwrite = true,
            "--tree" => options.tree = value("--tree")?,
            "--dir" => options.dir = Some(PathBuf::from(value("--dir")?)),
            _ => {
                if arg.starts_with('-') {
                    return Err(usage(format!("Unknown option for dream: {arg}")));
                }
                if subcommand.is_some() {
                    return Err(usage(format!(
                        "dream takes a single subcommand: unexpected {arg}"
                    )));
                }
                subcommand = Some(
                    DreamSubcommand::from_alias(arg)
                        .ok_or_else(|| usage(format!("Unknown dream subcommand: {arg}")))?,
                );
            }
        }
        index += 1;
    }
    options.subcommand = subcommand.unwrap_or(DreamSubcommand::Loop);
    if options.subcommand == DreamSubcommand::Experiment && iterations_given {
        return Err(usage(
            "experiment takes --rounds (rollouts per arm), not --iterations.",
        ));
    }
    if let Some(n) = options.n {
        if let Err(error) = resolve_task(options.task, Some(n)) {
            return Err(usage(format!("--n: {}", error.0)));
        }
    }
    Ok(options)
}

/// Where the command writes its output lines.
pub trait DreamCommandIo {
    /// One stdout line.
    fn stdout(&mut self, line: &str);
    /// One stderr line.
    fn stderr(&mut self, line: &str);
    /// The frozen run clock (ms); the wall clock in the binary.
    fn now(&self) -> u64;
}

/// How an invocation that parsed ended, for adoption telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DreamRunOutcome {
    /// The subcommand ran to completion (exit 0).
    Completed,
    /// It ran and failed: an empty store, an existing experiment, a store error (exit 2).
    Failed,
    /// It asked for what the standalone runner cannot serve: an LLM flag or a guided arm (exit 2).
    Unavailable,
}

impl DreamRunOutcome {
    /// The telemetry vocabulary.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unavailable => "unavailable",
        }
    }
}

/// What an invocation reports for adoption telemetry: counts and vocabulary only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamRunReport {
    pub subcommand: DreamSubcommand,
    pub task: DreamTaskId,
    pub outcome: DreamRunOutcome,
    /// Rollouts the run grew (0 for a read-only subcommand).
    pub rollouts: u64,
    /// Probes (evaluated attempts) the run spent.
    pub probes: u64,
    /// Whether the run adopted or selected a strictly better policy.
    pub improved: bool,
}

/// The command's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DreamCommandOutcome {
    pub exit_code: i32,
    /// Absent only when the arguments did not parse.
    pub report: Option<DreamRunReport>,
}

/// The counts a completed subcommand reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RunCounts {
    rollouts: u64,
    probes: u64,
    improved: bool,
}

fn score(value: f64) -> String {
    if value.is_finite() {
        to_fixed(value, 6)
    } else {
        "-".to_string()
    }
}

fn signed(value: f64) -> String {
    format!("{}{}", if value >= 0.0 { "+" } else { "" }, score(value))
}

fn multiplier(value: Option<f64>) -> String {
    value.map_or_else(
        || "-".to_string(),
        |value| format!("{}x", to_fixed(value, 2)),
    )
}

fn task_n(options: &DreamCommandOptions) -> Option<usize> {
    resolve_task_n(options.task, options.n)
}

fn n_suffix(n: Option<usize>) -> String {
    n.map_or_else(String::new, |n| format!(" n {n}"))
}

fn priming_policies(options: &DreamCommandOptions) -> Vec<ExplorationPolicy> {
    match options.priming {
        DreamPriming::None => Vec::new(),
        DreamPriming::Diverse => PRIMING_DIVERSE.to_vec(),
    }
}

fn betas(objective: &ReplayObjectiveConfig) -> String {
    format!(
        "beta1 {}  beta2 {}  beta3 {}",
        js_number(objective.beta1),
        js_number(objective.beta2),
        js_number(objective.beta3)
    )
}

fn resolve_tree_id(store: &Path, requested: &str) -> Option<String> {
    let summaries = list_trees(store);
    if requested == "latest" {
        return summaries
            .iter()
            .reduce(|latest, summary| {
                if summary.created_ts > latest.created_ts
                    || (summary.created_ts == latest.created_ts && summary.tree_id > latest.tree_id)
                {
                    summary
                } else {
                    latest
                }
            })
            .map(|summary| summary.tree_id.clone());
    }
    summaries
        .iter()
        .any(|summary| summary.tree_id == requested)
        .then(|| requested.to_string())
}

fn missing_tree(io: &mut dyn DreamCommandIo, store: &Path, requested: &str) -> i32 {
    let what = if requested == "latest" {
        "recorded trees".to_string()
    } else {
        format!("tree {requested}")
    };
    io.stderr(&format!("Error: no {what} in {}", store.display()));
    2
}

fn store_error(io: &mut dyn DreamCommandIo, error: &DreamStoreError) -> i32 {
    io.stderr(&format!("Error: {error}"));
    2
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn to_value(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// `{...result, dir, rounds}`: the loop result with its round lines, then `dir`.
fn loop_json(result: &DreamLoopResult, store: &Path) -> Value {
    let rounds: Vec<Value> = result
        .rounds
        .iter()
        .map(|record| {
            serde_json::json!({
                "iteration": record.iteration,
                "treeId": record.tree_id,
                "bestScore": json::number(record.round_best),
                "probes": record.probes,
            })
        })
        .collect();
    let mut map = object(to_value(result));
    map.insert("rounds".into(), Value::Array(rounds));
    map.insert("dir".into(), Value::from(store.to_string_lossy().as_ref()));
    Value::Object(map)
}

fn run_loop(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
    clock: &dyn Fn() -> u64,
) -> Result<RunCounts, i32> {
    let n = task_n(options);
    let task = resolve_task(options.task, n).map_err(|_| 1)?;
    let result = run_dream_loop(DreamLoopOptions {
        task: task.as_ref(),
        task_id: options.task.as_str().to_string(),
        n: n.and_then(|n| u32::try_from(n).ok()),
        seed: Seed::from(options.seed),
        clock,
        workers: options.workers,
        k1: options.k1,
        k2: options.k2,
        dreams: usize::try_from(options.dreams).unwrap_or(usize::MAX),
        iterations: options.iterations,
        dir: store,
        objective: options.objective,
        rng: None,
        initial_policy: DEFAULT_POLICY,
        fixed_policy: false,
        candidates: None,
        run_label: None,
        dreams_log_context: DreamsLogContext::default(),
        priming_policies: priming_policies(options),
    })
    .map_err(|error| store_error(io, &error))?;
    let summary = RunCounts {
        rollouts: result.rounds.len() as u64,
        probes: result
            .rounds
            .iter()
            .map(|record| u64::from(record.probes))
            .sum(),
        improved: result.improved,
    };
    if options.json {
        io.stdout(&json::stringify_pretty(&loop_json(&result, store)));
        return Ok(summary);
    }
    io.stdout(&format!("dream loop  {}", store.display()));
    io.stdout(&format!(
        "  task {}{}  seed {}  mode local  W {}  k1 {}  k2 {}  M {}  {}  iterations {}",
        result.task,
        n_suffix(n),
        result.seed,
        options.workers,
        options.k1,
        options.k2,
        options.dreams,
        betas(&options.objective),
        result.iterations
    ));
    for record in &result.rounds {
        io.stdout(&format!(
            "  round {}: best {}  probes {}  tree {}",
            record.iteration,
            score(record.round_best),
            record.probes,
            record.tree_id
        ));
    }
    for record in &result.rounds {
        let Some(probation) = record
            .dreaming
            .as_ref()
            .and_then(|dreaming| dreaming.probation.as_ref())
        else {
            continue;
        };
        io.stdout(&format!(
            "  probation {}: policy {} {}  rollout best {}  floor {}  charged {}/{} vs incumbent {}/{}  evidence trees {}",
            record.iteration,
            probation.policy_id,
            if probation.reverted { "REVERTED" } else { "kept" },
            score(probation.round_best),
            score(probation.floor),
            js_number(probation.charged_probes),
            js_number(probation.charged_rounds),
            js_number(probation.incumbent_charged_probes),
            js_number(probation.incumbent_charged_rounds),
            probation.evidence_trees
        ));
    }
    io.stdout(&format!(
        "  initial policy {}  score {}",
        result.initial_policy_id,
        score(result.initial_policy_score)
    ));
    io.stdout(&format!(
        "  final   policy {}  score {}  improved {}",
        result.final_policy_id,
        score(result.final_policy_score),
        result.improved
    ));
    io.stdout(&format!(
        "  best node score {}  tokens {}",
        score(result.best_node_score),
        result.tokens
    ));
    Ok(summary)
}

fn dreaming_line(row: &ExperimentRoundRow) -> Option<String> {
    let dreaming = row.dreaming.as_ref()?;
    let verdicts = &dreaming.candidate_verdicts;
    let eligible = verdicts.iter().filter(|verdict| verdict.eligible).count();
    let winner = verdicts
        .iter()
        .find(|verdict| verdict.reason == CandidateReason::Winner);
    let lever = dreaming
        .lever_scan
        .as_ref()
        .map_or_else(String::new, |scan| {
            format!(
                "  lever gap {} ({} policies, {} eligible)",
                signed(scan.gap),
                scan.policies,
                scan.eligible
            )
        });
    let probation = dreaming
        .probation
        .as_ref()
        .map_or_else(String::new, |probation| {
            format!(
                "  probation {} (rollout best {} vs floor {})",
                if probation.reverted {
                    "REVERTED"
                } else {
                    "kept"
                },
                score(probation.round_best),
                score(probation.floor)
            )
        });
    Some(format!(
        "dreaming: candidates {}  eligible {eligible}  {}  measured trees {}/{}{lever}  dreamer {}{probation}",
        verdicts.len(),
        winner.map_or_else(
            || "tie (current kept)".to_string(),
            |winner| format!("winner {}", winner.policy_id)
        ),
        dreaming.measured_trees,
        row.pool_size,
        dreaming.dreamer.as_str()
    ))
}

fn provenance_line(arm: &ExperimentArmResult) -> String {
    let totals = &arm.totals;
    let local = totals.probes - totals.agent_generated_calls;
    let probes = format!(
        "provenance: {} probes = {} agent-generated + {local} local ({} fallbacks)",
        totals.probes, totals.agent_generated_calls, totals.local_fallbacks
    );
    if totals.llm_proposals == 0 {
        return format!("{probes}; local proposer, 0 LLM proposals");
    }
    let rejected: Vec<String> = totals
        .llm_rejected
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(reason, count)| format!("{} {count}", reason.as_str()))
        .collect();
    let detail = if rejected.is_empty() {
        String::new()
    } else {
        format!(" ({})", rejected.join(", "))
    };
    format!(
        "{probes}; {} LLM proposals = {} accepted + {} rejected{detail}",
        totals.llm_proposals,
        totals.llm_accepted,
        totals.llm_rejected.total()
    )
}

fn print_arm(arm: &ExperimentArmResult, io: &mut dyn DreamCommandIo) {
    io.stdout(&format!(
        "  arm {}  proposer {}  dreamer {}  fixed {}  guided {}  run {}",
        arm.arm.as_str(),
        arm.mode.proposer,
        arm.mode.dreamer,
        arm.fixed_policy,
        arm.guided,
        arm.run_id
    ));
    io.stdout("    round | best | cum best | probes | agent | fallback | cum probes | policy");
    for row in &arm.rounds {
        let dreamed = row.dreaming.as_ref().map_or_else(String::new, |dreaming| {
            format!(
                "  dreamed {} -> {} improved {}",
                score(dreaming.current_score),
                score(dreaming.chosen_score),
                dreaming.improved
            )
        });
        io.stdout(&format!(
            "    {:>5} | {} | {} | {:>6} | {:>5} | {:>8} | {:>10} | {}{dreamed}",
            row.round,
            score(row.round_best),
            score(row.cumulative_best),
            row.probes,
            row.agent_generated_calls,
            row.local_fallbacks,
            row.cumulative_probes,
            row.policy_id
        ));
        if let Some(line) = dreaming_line(row) {
            io.stdout(&format!("      {line}"));
        }
    }
    io.stdout(&format!(
        "    final policy {}  changes {}  selected policy {}  own-pool score {} -> {}  final best {}  probes {}  handler calls {}  tokens {}",
        arm.final_policy_id,
        arm.policy_changes,
        arm.selected_policy_id,
        score(arm.policy_score_on_own_pool.initial),
        score(arm.policy_score_on_own_pool.final_score),
        score(arm.totals.final_best),
        arm.totals.probes,
        arm.totals.handler_calls,
        arm.totals.tokens
    ));
    io.stdout(&format!("    {}", provenance_line(arm)));
    let phases: Vec<&ExperimentRoundRow> = arm
        .rounds
        .iter()
        .filter(|row| row.dreaming.is_some())
        .collect();
    if !phases.is_empty() {
        let improved = phases
            .iter()
            .filter(|row| {
                row.dreaming
                    .as_ref()
                    .is_some_and(|dreaming| dreaming.improved)
            })
            .count();
        let inert = if arm.policy_changes == 0 {
            "  -> INERT (the arm ran its initial policy throughout)"
        } else {
            ""
        };
        io.stdout(&format!(
            "    dreaming: {} phases  improved {improved}/{}  policy changes {}{inert}",
            phases.len(),
            phases.len(),
            arm.policy_changes
        ));
    }
}

fn arm_number(map: &Map<String, Value>, arm: &str) -> Option<f64> {
    map.get(arm).and_then(Value::as_f64)
}

fn print_headline(result: &ExperimentResult, io: &mut dyn DreamCommandIo) {
    let Some(headline) = &result.headline else {
        io.stdout("  headline: not comparable (no fixed arm)");
        return;
    };
    let reference = headline.reference;
    let fixed_probes = arm_number(&headline.probes_to_target, reference)
        .map_or_else(String::new, |probes| {
            format!(" reached at {} probes", js_number(probes))
        });
    io.stdout(&format!(
        "  headline vs {reference}: target {}{fixed_probes}; equal budget {} probes",
        score(headline.target),
        headline.equal_budget
    ));
    for arm in &result.arms {
        let name = arm.arm.as_str();
        if name == reference {
            continue;
        }
        let reach = match arm_number(&headline.probes_to_target, name) {
            None => "target not reached".to_string(),
            Some(probes) => format!(
                "target at {} probes -> {}",
                js_number(probes),
                arm_number(&headline.calls_multiplier, name).map_or_else(
                    || "not comparable".to_string(),
                    |calls| format!("{} fewer calls", multiplier(Some(calls)))
                )
            ),
        };
        let budget = match arm_number(&headline.best_at_budget, name) {
            None => "at equal budget: not comparable".to_string(),
            Some(best) => format!(
                "at equal budget: {} vs {} -> {}",
                score(best),
                score(arm_number(&headline.best_at_budget, reference).unwrap_or(f64::NAN)),
                arm_number(&headline.score_multiplier, name).map_or_else(
                    || "not comparable".to_string(),
                    |ratio| format!("{} score", multiplier(Some(ratio)))
                )
            ),
        };
        let delta = arm_number(&headline.delta_best, name).unwrap_or(0.0);
        io.stdout(&format!(
            "    {name}: {reach}; {budget}; delta best {}",
            signed(delta)
        ));
    }
    let fixed_exact = arm_number(&headline.probes_to_target_exact, reference);
    io.stdout(&format!(
        "  exact headline (probe-granular): target{}",
        fixed_exact.map_or_else(
            || " not reached by the reference".to_string(),
            |probe| format!(" reached by {reference} at probe {}", js_number(probe))
        )
    ));
    for arm in &result.arms {
        let name = arm.arm.as_str();
        if name == reference {
            continue;
        }
        let line = match arm_number(&headline.probes_to_target_exact, name) {
            None => "target not reached".to_string(),
            Some(probe) => format!(
                "target at probe {} -> {}",
                js_number(probe),
                arm_number(&headline.calls_multiplier_exact, name).map_or_else(
                    || "not comparable".to_string(),
                    |calls| format!("{} fewer calls", multiplier(Some(calls)))
                )
            ),
        };
        io.stdout(&format!("    {name}: {line}"));
    }
}

fn print_experiment(result: &ExperimentResult, io: &mut dyn DreamCommandIo, store: &Path) {
    let budget = &result.budget;
    io.stdout(&format!("dream experiment  {}", result.experiment_id));
    let arms: Vec<&str> = result.arms.iter().map(|arm| arm.arm.as_str()).collect();
    io.stdout(&format!(
        "  task {}{}  scoring {}  seed {}  rounds {}  W {}  k1 {}  k2 {}  M {}  {}  arms {}",
        result.task.0,
        n_suffix(result.n),
        result.scoring,
        result.seed,
        result.rounds,
        budget.workers,
        budget.k1,
        budget.k2,
        budget.dreams,
        betas(&result.objective),
        arms.join(",")
    ));
    io.stdout(&format!("  initial policy {}", result.initial_policy_id));
    for arm in &result.arms {
        print_arm(arm, io);
    }
    print_headline(result, io);
    for note in &result.notes {
        io.stdout(&format!("  note: {note}"));
    }
    io.stdout(&format!("  store {}", store.display()));
    io.stdout(&format!(
        "  results {}",
        crate::store::experiment_result_path(store, &result.experiment_id).display()
    ));
}

fn run_experiment_command(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
    clock: &dyn Fn() -> u64,
) -> Result<RunCounts, i32> {
    let seeds = options.seeds.clone().unwrap_or_else(|| vec![options.seed]);
    let mut results = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let mut spec = ExperimentSpec::new(
            options.task,
            Seed::from(seed),
            options.rounds,
            ExperimentBudget {
                workers: options.workers,
                k1: options.k1,
                k2: options.k2,
                dreams: options.dreams,
            },
            options.arms.clone(),
        );
        spec.n = task_n(options);
        spec.objective = options.objective;
        spec.priming_policies = priming_policies(options);
        let result = match run_experiment(
            &spec,
            &ExperimentRunOptions {
                dir: store,
                clock,
                notes: Vec::new(),
                overwrite: options.overwrite,
            },
        ) {
            Ok(result) => result,
            Err(
                error @ (ExperimentError::ArmUnavailable(_)
                | ExperimentError::Range(_)
                | ExperimentError::Store(_)),
            ) => {
                io.stderr(&format!("Error: {error}"));
                return Err(2);
            }
        };
        if !options.json {
            print_experiment(&result, io, store);
        }
        results.push(result);
    }
    if options.json {
        let text = if options.seeds.is_some() {
            json::stringify_pretty(&results)
        } else {
            results
                .first()
                .map(json::stringify_pretty)
                .unwrap_or_default()
        };
        io.stdout(&text);
    }
    Ok(RunCounts {
        rollouts: results
            .iter()
            .flat_map(|result| &result.arms)
            .map(|arm| arm.rounds.len() as u64)
            .sum(),
        probes: results
            .iter()
            .flat_map(|result| &result.arms)
            .map(|arm| u64::from(arm.totals.probes))
            .sum(),
        improved: results
            .iter()
            .flat_map(|result| &result.arms)
            .any(|arm| arm.policy_changes > 0),
    })
}

fn run_rollout(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
    clock: &dyn Fn() -> u64,
) -> Result<RunCounts, i32> {
    let n = task_n(options);
    let task = resolve_task(options.task, n).map_err(|_| 1)?;
    let seed = Seed::from(options.seed);
    let result = run_online_exploration(ExploreOptions {
        task: task.as_ref(),
        task_id: options.task.as_str().to_string(),
        n: n.and_then(|n| u32::try_from(n).ok()),
        rng: SeededRng::new(&seed),
        seed,
        clock,
        workers: options.workers,
        k1: options.k1,
        dir: store,
        policy: DEFAULT_POLICY,
        iteration: 0,
        proposer: None,
        tree_id: None,
        cancel: None,
    })
    .map_err(|error| store_error(io, &error))?;
    let default_id = policy_id(&DEFAULT_POLICY);
    if options.json {
        let value = serde_json::json!({
            "treeId": result.tree_id,
            "policyId": default_id,
            "rounds": result.rounds,
            "revealedCount": result.revealed_count,
            "bestScore": json::number(result.best_score),
            "bestNodeId": result.best_node_id,
            "rootScore": json::number(result.root_score),
            "tokens": result.tokens,
            "dir": store.to_string_lossy(),
        });
        io.stdout(&json::stringify_pretty(&value));
    } else {
        io.stdout(&format!("dream rollout  {}", store.display()));
        io.stdout(&format!(
            "  tree {}  task {}{}  policy {default_id}",
            result.tree_id,
            options.task,
            n_suffix(n)
        ));
        io.stdout(&format!(
            "  rounds {}  revealed {}  best {}  best node {}  tokens {}",
            result.rounds,
            result.revealed_count,
            score(result.best_score),
            result.best_node_id,
            result.tokens
        ));
    }
    Ok(RunCounts {
        rollouts: 1,
        probes: u64::from(result.revealed_count),
        improved: false,
    })
}

fn run_replay(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
) -> Result<RunCounts, i32> {
    let Some(tree_id) = resolve_tree_id(store, &options.tree) else {
        return Err(missing_tree(io, store, &options.tree));
    };
    let recorded = read_tree(&tree_id, store).map_err(|error| store_error(io, &error))?;
    let result = simulate_policy_with_span(
        &recorded,
        &DEFAULT_POLICY,
        options.k1,
        options.k2,
        &options.objective,
    );
    let v = compute_objective(
        &result,
        &options.objective,
        pool_score_scale([&recorded]),
        ObjectiveBudget {
            workers: recorded.header.w,
            k1: options.k1,
        },
    );
    if options.json {
        let mut map = object(to_value(&result));
        map.insert("v".into(), json::number(v));
        io.stdout(&json::stringify_pretty(&Value::Object(map)));
    } else {
        io.stdout(&format!("dream replay  {}", result.tree_id));
        io.stdout(&format!("  policy {}", result.policy_id));
        io.stdout(&format!(
            "  revealed N {}  rounds {}  best {}  out-of-support {}  in-support {}  probes to best {}",
            result.n,
            result.rounds,
            score(result.best_score),
            result.out_of_support_cells,
            score(result.in_support),
            result.probes_to_best
        ));
        io.stdout(&format!(
            "  V {}  ({}  budget {}x{})",
            score(v),
            betas(&options.objective),
            recorded.header.w,
            options.k1
        ));
    }
    Ok(RunCounts {
        rollouts: 0,
        probes: 0,
        improved: false,
    })
}

fn run_improve(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
) -> Result<RunCounts, i32> {
    let summaries: Vec<TreeSummary> = list_trees(store)
        .into_iter()
        .filter(|summary| summary.task_id == options.task.as_str())
        .collect();
    if summaries.is_empty() {
        io.stderr(&format!(
            "Error: no recorded {} trees in {}; run \"dream rollout\" first",
            options.task,
            store.display()
        ));
        return Err(2);
    }
    let pool = summaries
        .iter()
        .map(|summary| read_tree(&summary.tree_id, store))
        .collect::<Result<Vec<RecordedTree>, _>>()
        .map_err(|error| store_error(io, &error))?;
    let revoked = std::collections::HashSet::new();
    let result = run_dreaming(DreamingOptions {
        current: DEFAULT_POLICY,
        pool: &pool,
        dreams: usize::try_from(options.dreams).unwrap_or(usize::MAX),
        k1: options.k1,
        k2: options.k2,
        rng: SeededRng::new(&Seed::from(options.seed)),
        objective: options.objective,
        quality_eps: 0.0,
        iteration: 0,
        candidates: None,
        lever_scan: true,
        revoked: &revoked,
    });
    let current_id = policy_id(&DEFAULT_POLICY);
    if options.json {
        let mut map = object(to_value(&result));
        map.insert("currentPolicyId".into(), Value::from(current_id.as_str()));
        map.insert("dir".into(), Value::from(store.to_string_lossy().as_ref()));
        io.stdout(&json::stringify_pretty(&Value::Object(map)));
    } else {
        io.stdout(&format!("dream improve  {}", store.display()));
        io.stdout(&format!(
            "  pool {}  measured trees {}  evidence trees {}  candidates {}  simulations {}",
            result.pool_size,
            result.measured_trees,
            result.evidence_trees,
            result.scored_count,
            result.simulations
        ));
        io.stdout(&format!(
            "  current policy {current_id}  current score {}  quality {}",
            score(result.current_score),
            score(result.current_quality)
        ));
        io.stdout(&format!(
            "  chosen  policy {}  chosen score {}  quality {}  improved {}  quality-rejected {}",
            result.chosen_policy_id,
            score(result.chosen_score),
            score(result.chosen_quality),
            result.improved,
            result.quality_rejected
        ));
        for candidate in &result.candidate_policy_ids {
            io.stdout(&format!("    candidate {candidate}"));
        }
    }
    Ok(RunCounts {
        rollouts: 0,
        probes: 0,
        improved: result.improved,
    })
}

fn run_status(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
) -> Result<RunCounts, i32> {
    let summaries = list_trees(store);
    let experiment_ids = list_experiment_ids(store);
    let mut task_counts = Map::new();
    let mut best: Option<f64> = None;
    let mut latest: Option<&TreeSummary> = None;
    for summary in &summaries {
        let count = task_counts
            .get(&summary.task_id)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        task_counts.insert(summary.task_id.clone(), Value::from(count + 1));
        if summary.best_score.is_finite() && best.is_none_or(|current| summary.best_score > current)
        {
            best = Some(summary.best_score);
        }
        if latest.is_none_or(|current| summary.created_ts > current.created_ts) {
            latest = Some(summary);
        }
    }
    let status = serde_json::json!({
        "dir": store.to_string_lossy(),
        "treeCount": summaries.len(),
        "taskCounts": task_counts,
        "bestNodeScore": json::number(best.unwrap_or(0.0)),
        "lastPolicyId": latest.map(|summary| summary.policy_id.clone()),
        "experimentCount": experiment_ids.len(),
        "experimentIds": experiment_ids,
    });
    if summaries.is_empty() && experiment_ids.is_empty() {
        if options.json {
            io.stdout(&json::stringify_pretty(&status));
        } else {
            io.stderr(&format!(
                "Error: dream store is empty ({})",
                store.display()
            ));
        }
        return Err(2);
    }
    if options.json {
        io.stdout(&json::stringify_pretty(&status));
    } else {
        io.stdout(&format!("dream store  {}", store.display()));
        io.stdout(&format!(
            "  trees {}  best node score {}",
            summaries.len(),
            score(best.unwrap_or(0.0))
        ));
        for (task, count) in &task_counts {
            io.stdout(&format!("    {task}: {count}"));
        }
        io.stdout(&format!(
            "  last policy {}",
            latest.map_or("-", |summary| summary.policy_id.as_str())
        ));
        io.stdout(&format!("  experiments {}", experiment_ids.len()));
        for experiment_id in &experiment_ids {
            io.stdout(&format!("    {experiment_id}"));
        }
    }
    Ok(RunCounts {
        rollouts: 0,
        probes: 0,
        improved: false,
    })
}

fn show_json(recorded: &RecordedTree) -> Value {
    let nodes: Vec<Value> = recorded
        .nodes
        .iter()
        .map(|node| {
            let mut map = object(to_value(node));
            // `{...record, origin}`: a recorded origin keeps its place, a missing one is appended.
            map.insert("origin".into(), Value::from(node.origin().as_str()));
            Value::Object(map)
        })
        .collect();
    serde_json::json!({
        "header": to_value(&recorded.header),
        "nodes": nodes,
        "reveals": to_value(&recorded.reveals),
    })
}

fn run_show(
    options: &DreamCommandOptions,
    io: &mut dyn DreamCommandIo,
    store: &Path,
) -> Result<RunCounts, i32> {
    let Some(tree_id) = resolve_tree_id(store, &options.tree) else {
        return Err(missing_tree(io, store, &options.tree));
    };
    let recorded = read_tree(&tree_id, store).map_err(|error| store_error(io, &error))?;
    if options.json {
        io.stdout(&json::stringify_pretty(&show_json(&recorded)));
    } else {
        let header = &recorded.header;
        let agent = recorded
            .nodes
            .iter()
            .filter(|node| node.origin() == crate::records::NodeOrigin::Llm)
            .count();
        io.stdout(&format!("dream tree  {}", header.tree_id));
        io.stdout(&format!(
            "  task {}{}  W {}  seed {}  policy {}  iteration {}  agent-generated {agent}/{}",
            header.task_id,
            header.n.map_or_else(String::new, |n| format!(" n {n}")),
            header.w,
            header.seed,
            header.policy_id,
            header.iteration,
            recorded.nodes.len().saturating_sub(1)
        ));
        for reveal in &recorded.reveals {
            io.stdout(&format!(
                "  round {}: reveal {}",
                reveal.round,
                reveal.ids.join(", ")
            ));
        }
        for node in &recorded.nodes {
            io.stdout(&format!(
                "  {}  parent {}  branch {}  seq {}  round {}  score {}  valid {}  origin {}{}",
                node.id,
                node.parent_id.as_deref().unwrap_or("-"),
                node.branch,
                node.seq,
                node.round,
                score(node.score),
                node.valid,
                node.origin().as_str(),
                node.fail_class
                    .as_deref()
                    .filter(|class| !class.is_empty())
                    .map_or_else(String::new, |class| format!("  fail {class}"))
            ));
        }
    }
    Ok(RunCounts {
        rollouts: 0,
        probes: 0,
        improved: false,
    })
}

/// Run `prime-agent dream <args>`.
#[must_use]
pub fn run_dream_command(args: &[String], io: &mut dyn DreamCommandIo) -> DreamCommandOutcome {
    let options = match parse_dream_command_args(args) {
        Ok(options) => options,
        Err(error) => {
            io.stderr(&format!("Error: {error}"));
            io.stderr(&format!("Usage: prime-agent {DREAM_USAGE}"));
            return DreamCommandOutcome {
                exit_code: 1,
                report: None,
            };
        }
    };
    let report = |outcome: DreamRunOutcome, counts: RunCounts| DreamRunReport {
        subcommand: options.subcommand,
        task: options.task,
        outcome,
        rollouts: counts.rollouts,
        probes: counts.probes,
        improved: counts.improved,
    };
    let unavailable = |io: &mut dyn DreamCommandIo, message: &str| {
        io.stderr(message);
        DreamCommandOutcome {
            exit_code: 2,
            report: Some(report(DreamRunOutcome::Unavailable, RunCounts::default())),
        }
    };
    if options.llm_proposer || options.llm_dreamer {
        return unavailable(io, LLM_REJECTION_MESSAGE);
    }
    if options.subcommand == DreamSubcommand::Experiment
        && options.arms.iter().any(|arm| arm.guided())
    {
        return unavailable(io, GUIDED_ARM_REJECTION_MESSAGE);
    }
    let now = io.now();
    let clock = move || now;
    let store = options.dir.clone().unwrap_or_else(dream_dir);
    let outcome = match options.subcommand {
        DreamSubcommand::Loop => run_loop(&options, io, &store, &clock),
        DreamSubcommand::Experiment => run_experiment_command(&options, io, &store, &clock),
        DreamSubcommand::Rollout => run_rollout(&options, io, &store, &clock),
        DreamSubcommand::Replay => run_replay(&options, io, &store),
        DreamSubcommand::Improve => run_improve(&options, io, &store),
        DreamSubcommand::Status => run_status(&options, io, &store),
        DreamSubcommand::Show => run_show(&options, io, &store),
    };
    match outcome {
        Ok(counts) => DreamCommandOutcome {
            exit_code: 0,
            report: Some(report(DreamRunOutcome::Completed, counts)),
        },
        Err(exit_code) => DreamCommandOutcome {
            exit_code,
            report: Some(report(DreamRunOutcome::Failed, RunCounts::default())),
        },
    }
}
