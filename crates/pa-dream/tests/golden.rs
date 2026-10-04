//! Cross-language parity: every golden under `tests/fixtures/golden` was
//! produced by the TS product (`perf/session-catalog-resume`, `core/dream` and
//! `cli/dream-command.ts` bundled with esbuild and run on Node's V8) with the
//! same seeds, budgets and frozen clock. Each test re-runs the scenario in Rust
//! and compares the whole result and the sha256 of every file the run wrote:
//! trees, blobs, dreams logs and `result.json` are byte-identical.

// Exact float equality is the claim: replay and the objective are deterministic
// IEEE-754 arithmetic, and the expected values are exact or recorded.
#![allow(clippy::float_cmp)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pa_dream::command::{run_dream_command, DreamCommandIo};
use pa_dream::dream_loop::{run_dream_loop, DreamLoopOptions};
use pa_dream::dreams::DreamsLogContext;
use pa_dream::experiment::{
    run_experiment, ExperimentArm, ExperimentBudget, ExperimentRunOptions, ExperimentSpec,
};
use pa_dream::json;
use pa_dream::objective::DEFAULT_OBJECTIVE;
use pa_dream::policy::{sha256_hex, ExplorationPolicy, DEFAULT_POLICY, PRIMING_DIVERSE};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::rollout::{run_online_exploration, ExploreOptions};
use pa_dream::tasks::{resolve_task, DreamTaskId};
use serde_json::{json, Value};

const CLOCK: u64 = 1_700_000_000_000;

fn golden(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden")
        .join(name);
    let text = std::fs::read_to_string(&path).expect("golden file");
    json::parse(&text).expect("golden JSON")
}

/// sha256 of every file under `dir`, keyed by its `/`-separated relative path.
fn manifest(dir: &Path) -> Value {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let relative: Vec<String> = path
                    .strip_prefix(root)
                    .expect("under root")
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect();
                out.insert(
                    relative.join("/"),
                    sha256_hex(&std::fs::read(&path).expect("read")),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    serde_json::to_value(out).expect("manifest")
}

/// Compare as the TS printed it, so number formatting is part of the check.
fn assert_json_eq(actual: &Value, expected: &Value, what: &str) {
    let actual = json::stringify_pretty(actual);
    let expected = json::stringify_pretty(expected);
    if actual != expected {
        let line = actual
            .lines()
            .zip(expected.lines())
            .position(|(a, e)| a != e)
            .unwrap_or(0);
        let context = |text: &str| {
            text.lines()
                .skip(line.saturating_sub(3))
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        };
        panic!(
            "{what} differs from the TS golden at line {line}:\n--- rust\n{}\n--- ts\n{}",
            context(&actual),
            context(&expected)
        );
    }
}

#[test]
fn the_seeded_rng_draws_bit_identical_streams() {
    let draws = |seed: Seed| {
        let mut rng = SeededRng::new(&seed);
        let mut fork = rng.fork("cand:3");
        json!({
            "next": [rng.next(), rng.next(), rng.next()],
            "gaussian": [rng.next_gaussian(), rng.next_gaussian()],
            "int": [rng.next_int(81), rng.next_int(6)],
            "fork": [fork.next(), fork.next_gaussian(), fork.fork("retry:1").next()],
        })
    };
    let actual = json!({
        "seed7": draws(Seed::Number(7)),
        "seedText": draws(Seed::Text("abc".to_string())),
        "seed0": draws(Seed::Number(0)),
    });
    assert_json_eq(&actual, &golden("rng.json"), "rng draws");
}

#[test]
fn a_circle_packing_rollout_writes_the_ts_tree_byte_for_byte() {
    let dir = tempfile::tempdir().expect("tempdir");
    let task = resolve_task(DreamTaskId::CirclePacking, Some(26)).expect("task");
    let clock = || CLOCK;
    let result = run_online_exploration(ExploreOptions {
        task: task.as_ref(),
        task_id: "circle-packing".to_string(),
        n: Some(26),
        seed: Seed::Number(7),
        rng: SeededRng::new(&Seed::Number(7)),
        clock: &clock,
        workers: 4,
        k1: 12,
        dir: dir.path(),
        policy: DEFAULT_POLICY,
        iteration: 0,
        proposer: None,
        tree_id: None,
    })
    .expect("rollout");
    let expected = golden("rollout-circle-packing.json");
    let actual = json!({
        "result": {
            "treeId": result.tree_id,
            "rounds": result.rounds,
            "revealedCount": result.revealed_count,
            "bestScore": result.best_score,
            "bestNodeId": result.best_node_id,
            "probesToBest": result.probes_to_best,
            "improvements": result.improvements,
            "rootScore": result.root_score,
        },
        "files": manifest(dir.path()),
    });
    assert_json_eq(&actual, &expected, "rollout");
}

struct LoopCase {
    task: DreamTaskId,
    n: Option<usize>,
    seed: i64,
    workers: u32,
    k1: u32,
    k2: u32,
    dreams: usize,
    iterations: u32,
    priming: Vec<ExplorationPolicy>,
}

fn assert_loop_matches(case: &LoopCase, golden_name: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let task = resolve_task(case.task, case.n).expect("task");
    let clock = || CLOCK;
    let result = run_dream_loop(DreamLoopOptions {
        task: task.as_ref(),
        task_id: case.task.as_str().to_string(),
        n: case.n.and_then(|n| u32::try_from(n).ok()),
        seed: Seed::Number(case.seed),
        clock: &clock,
        workers: case.workers,
        k1: case.k1,
        k2: case.k2,
        dreams: case.dreams,
        iterations: case.iterations,
        dir: dir.path(),
        objective: DEFAULT_OBJECTIVE,
        rng: None,
        initial_policy: DEFAULT_POLICY,
        fixed_policy: false,
        candidates: None,
        run_label: None,
        dreams_log_context: DreamsLogContext::default(),
        priming_policies: case.priming.clone(),
    })
    .expect("loop");
    let actual = json!({
        "result": serde_json::to_value(&result).expect("result"),
        "files": manifest(dir.path()),
    });
    assert_json_eq(&actual, &golden(golden_name), golden_name);
}

#[test]
fn a_sum_difference_loop_matches_the_ts_run() {
    assert_loop_matches(
        &LoopCase {
            task: DreamTaskId::SumDifference,
            n: None,
            seed: 3,
            workers: 3,
            k1: 6,
            k2: 12,
            dreams: 8,
            iterations: 3,
            priming: Vec::new(),
        },
        "loop-sum-difference.json",
    );
}

#[test]
fn a_primed_circle_packing_loop_matches_the_ts_run() {
    assert_loop_matches(
        &LoopCase {
            task: DreamTaskId::CirclePacking,
            n: Some(8),
            seed: 7,
            workers: 3,
            k1: 6,
            k2: 12,
            dreams: 6,
            iterations: 2,
            priming: PRIMING_DIVERSE.to_vec(),
        },
        "loop-circle-packing-primed.json",
    );
}

#[test]
fn a_loop_that_adopts_mutated_policies_matches_the_ts_run() {
    assert_loop_matches(
        &LoopCase {
            task: DreamTaskId::CirclePacking,
            n: Some(10),
            seed: 8,
            workers: 3,
            k1: 8,
            k2: 16,
            dreams: 8,
            iterations: 4,
            priming: Vec::new(),
        },
        "loop-circle-packing-adopting.json",
    );
}

#[test]
fn the_default_cli_loop_matches_the_ts_run() {
    assert_loop_matches(
        &LoopCase {
            task: DreamTaskId::CirclePacking,
            n: Some(26),
            seed: 7,
            workers: 4,
            k1: 12,
            k2: 24,
            dreams: 16,
            iterations: 3,
            priming: Vec::new(),
        },
        "loop-default.json",
    );
}

#[test]
fn an_autocorrelation_loop_matches_the_ts_run() {
    assert_loop_matches(
        &LoopCase {
            task: DreamTaskId::Autocorrelation,
            n: Some(32),
            seed: 11,
            workers: 3,
            k1: 8,
            k2: 16,
            dreams: 6,
            iterations: 3,
            priming: Vec::new(),
        },
        "loop-autocorrelation.json",
    );
}

#[test]
fn an_experiment_writes_the_ts_result_and_stores() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = || CLOCK;
    let mut spec = ExperimentSpec::new(
        DreamTaskId::Autocorrelation,
        Seed::Number(7),
        3,
        ExperimentBudget {
            workers: 3,
            k1: 6,
            k2: 12,
            dreams: 6,
        },
        vec![ExperimentArm::Dream, ExperimentArm::Fixed],
    );
    spec.n = Some(32);
    let result = run_experiment(
        &spec,
        &ExperimentRunOptions {
            dir: dir.path(),
            clock: &clock,
            notes: Vec::new(),
            overwrite: false,
        },
    )
    .expect("experiment");
    let actual = json!({
        "result": serde_json::to_value(&result).expect("result"),
        "files": manifest(dir.path()),
    });
    assert_json_eq(
        &actual,
        &golden("experiment-autocorrelation.json"),
        "experiment",
    );
}

struct Capture {
    stdout: Vec<String>,
    stderr: Vec<String>,
}

impl DreamCommandIo for Capture {
    fn stdout(&mut self, line: &str) {
        self.stdout.push(line.to_string());
    }
    fn stderr(&mut self, line: &str) {
        self.stderr.push(line.to_string());
    }
    fn now(&self) -> u64 {
        CLOCK
    }
}

#[test]
fn every_cli_transcript_matches_the_ts_command() {
    let base = tempfile::tempdir().expect("tempdir");
    let base_text = base.path().to_string_lossy().into_owned();
    let expected = golden("cli.json");
    let cases = expected.as_array().expect("transcripts");
    for case in cases {
        let args: Vec<String> = case["args"]
            .as_array()
            .expect("args")
            .iter()
            .map(|arg| arg.as_str().expect("arg").replace("<DIR>", &base_text))
            .collect();
        let mut io = Capture {
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let outcome = run_dream_command(&args, &mut io);
        let scrub = |lines: &[String]| -> Vec<String> {
            lines
                .iter()
                .map(|line| line.replace(&base_text, "<DIR>"))
                .collect()
        };
        let actual = json!({
            "args": case["args"],
            "exit": outcome.exit_code,
            "stdout": scrub(&io.stdout),
            "stderr": scrub(&io.stderr),
        });
        assert_json_eq(&actual, case, &format!("dream {}", args.join(" ")));
    }
}

#[test]
fn math_log_and_cos_round_exactly_like_v8() {
    let expected = golden("math.json");
    for (name, function) in [
        ("log", pa_dream::js_math::log as fn(f64) -> f64),
        ("cos", pa_dream::js_math::cos),
    ] {
        for pair in expected[name].as_array().expect("pairs") {
            let input = pair[0].as_f64().expect("input");
            let output = pair[1].as_f64().expect("output");
            assert_eq!(
                function(input).to_bits(),
                output.to_bits(),
                "{name}({input:e}) = {:e}, V8 {output:e}",
                function(input)
            );
        }
    }
}
