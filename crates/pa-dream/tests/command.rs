//! The TS `dream-command.test.ts` parser and usage behaviour (the run
//! transcripts themselves are byte-compared in `golden.rs`).

// Exact float equality is the claim: replay and the objective are deterministic
// IEEE-754 arithmetic, and the expected values are exact or recorded.
#![allow(clippy::float_cmp)]

use pa_dream::command::{
    DREAM_OPTIONS,
    DREAM_USAGE,
    DreamCommandIo,
    DreamCommandOptions,
    DreamCommandUsageError,
    DreamPriming,
    DreamRunOutcome,
    DreamRunReport,
    DreamSubcommand,
    parse_dream_command_args,
    run_dream_command,
};
use pa_dream::experiment::ExperimentArm;
use pa_dream::objective::ReplayObjectiveConfig;
use pa_dream::tasks::{DREAM_TASK_IDS, DreamTaskId};

fn parse(args: &[&str]) -> Result<DreamCommandOptions, DreamCommandUsageError> {
    let args: Vec<String> = args.iter().map(ToString::to_string).collect();
    parse_dream_command_args(&args)
}

fn ok(args: &[&str]) -> DreamCommandOptions {
    parse(args).expect("parses")
}

fn message(args: &[&str]) -> String {
    parse(args).expect_err("rejected").0
}

#[test]
fn defaults_aliases_and_both_value_forms() {
    let defaults = ok(&[]);
    assert_eq!(
        (
            defaults.subcommand,
            defaults.task,
            defaults.seed,
            defaults.iterations,
            defaults.workers
        ),
        (DreamSubcommand::Loop, DreamTaskId::CirclePacking, 1, 3, 4)
    );
    for (alias, subcommand) in [
        ("propose", DreamSubcommand::Rollout),
        ("simulate", DreamSubcommand::Replay),
        ("inspect", DreamSubcommand::Show),
        ("compare", DreamSubcommand::Experiment),
        ("rollout", DreamSubcommand::Rollout),
        ("status", DreamSubcommand::Status),
    ] {
        assert_eq!(ok(&[alias]).subcommand, subcommand);
    }
    assert_eq!(ok(&["--seed", "7"]).seed, 7);
    assert_eq!(ok(&["--seed=9"]).seed, 9);
    assert_eq!(
        ok(&["rollout", "--task=sum-difference"]).task,
        DreamTaskId::SumDifference
    );
    assert_eq!(ok(&["--n", "32"]).n, Some(32));
}

#[test]
fn experiment_options_parse_and_malformed_ones_are_usage_errors() {
    let options = ok(&["experiment", "--rounds", "3", "--arms", "dream,fixed"]);
    assert_eq!(
        (
            options.rounds,
            options.arms.clone(),
            options.seeds.clone(),
            options.overwrite
        ),
        (
            3,
            vec![ExperimentArm::Dream, ExperimentArm::Fixed],
            None,
            false
        )
    );
    assert_eq!(ok(&["experiment"]).rounds, 4);
    assert_eq!(
        ok(&["experiment", "--arms=fixed"]).arms,
        [ExperimentArm::Fixed]
    );
    assert_eq!(
        ok(&["experiment", "--seeds", "1,2,3"]).seeds,
        Some(vec![1, 2, 3])
    );
    assert!(ok(&["experiment", "--overwrite"]).overwrite);
    assert_eq!(
        ok(&["experiment", "--arms", "dream,dream-guided"]).arms,
        [ExperimentArm::Dream, ExperimentArm::DreamGuided]
    );
    for bad in [
        &["experiment", "--arms", "dream,nope"][..],
        &["experiment", "--arms", "dream,dream"],
        &["experiment", "--arms", ""],
        &["experiment", "--rounds", "0"],
        &["experiment", "--rounds", "2.5"],
        &["experiment", "--seeds", "1,1"],
        &["experiment", "--seeds", "1,-2"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
    assert!(message(&["experiment", "--iterations", "2"]).contains("takes --rounds"));
    assert_eq!(ok(&["loop", "--iterations", "2"]).iterations, 2);
}

#[test]
fn betas_priming_and_sizes_are_validated() {
    let betas = |beta1: f64, beta2: f64, beta3: f64| ReplayObjectiveConfig {
        beta1,
        beta2,
        beta3,
    };
    assert_eq!(ok(&[]).objective, betas(0.05, 0.1, 0.25));
    assert_eq!(
        ok(&["--beta1", "0.2", "--beta2=0"]).objective,
        betas(0.2, 0.0, 0.25)
    );
    assert_eq!(ok(&["--beta3", "0"]).objective.beta3, 0.0);
    assert_eq!(ok(&["--beta3=1"]).objective.beta3, 1.0);
    assert!(message(&["--beta3", "1.5"]).contains("[0, 1]"));
    assert_eq!(
        ok(&["experiment", "--beta1", "1e-3"]).objective.beta1,
        0.001
    );
    for bad in [
        &["--beta1", "-0.1"][..],
        &["--beta2", "nan"],
        &["--beta1", "Infinity"],
        &["--beta1", ""],
        &["--beta1"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
    assert_eq!(ok(&[]).priming, DreamPriming::None);
    assert_eq!(
        ok(&["experiment", "--priming=diverse"]).priming,
        DreamPriming::Diverse
    );
    assert!(message(&["--priming", "lots"]).contains("none or diverse"));
    assert!(parse(&["--priming"]).is_err());
    assert_eq!(ok(&["--n", "10"]).n, Some(10));
    assert_eq!(
        ok(&["--n", "128", "--task", "autocorrelation"]).n,
        Some(128)
    );
    assert!(
        message(&["--task", "autocorrelation", "--n", "10"]).starts_with("--n: autocorrelation")
    );
    assert!(message(&["--n", "1"]).starts_with("--n: circle-packing"));
    for bad in [
        &["--n", "0"][..],
        &["--n", "2.5"],
        &["--nope"],
        &["frobnicate"],
        &["--task", "banana"],
        &["--iterations", "1.5"],
        &["--workers", "0"],
        &["loop", "rollout"],
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}

fn flags(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("--") {
        let tail = &rest[start + 2..];
        let end = tail
            .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            .unwrap_or(tail.len());
        let flag = format!("--{}", &tail[..end]);
        if !out.contains(&flag) {
            out.push(flag);
        }
        rest = &tail[end..];
    }
    out
}

#[test]
fn the_usage_names_every_task_and_has_one_option_row_per_flag_the_parser_accepts() {
    let ids: Vec<&str> = DREAM_TASK_IDS.iter().map(|id| id.as_str()).collect();
    assert!(DREAM_USAGE.contains(&format!("--task <{}>", ids.join("|"))));
    let mut usage_flags = flags(DREAM_USAGE);
    assert!(usage_flags.len() > 10);
    let mut row_flags: Vec<String> = DREAM_OPTIONS
        .iter()
        .map(|row| flags(row.split_whitespace().next().unwrap_or_default()).remove(0))
        .collect();
    usage_flags.sort();
    row_flags.sort();
    assert_eq!(row_flags, usage_flags);
    let task_row = DREAM_OPTIONS
        .iter()
        .find(|row| row.starts_with("--task "))
        .expect("task row");
    assert!(ids.iter().all(|id| task_row.contains(id)));
    for flag in &usage_flags {
        let takes_value = DREAM_USAGE.contains(&format!("[{flag} <"));
        let args: Vec<&str> = if takes_value {
            vec![flag, "1"]
        } else {
            vec![flag]
        };
        if let Err(error) = parse(&args) {
            assert!(!error.0.contains("Unknown option"), "{flag}: {}", error.0);
        }
    }
}

struct Quiet;

impl DreamCommandIo for Quiet {
    fn stdout(&mut self, _line: &str) {}
    fn stderr(&mut self, _line: &str) {}
    fn now(&self) -> u64 {
        1_700_000_000_000
    }
}

#[test]
fn every_parsed_invocation_reports_its_outcome_and_counts_for_telemetry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = dir.path().to_string_lossy().into_owned();
    let run = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        run_dream_command(&args, &mut Quiet)
    };
    let report = |subcommand, task, outcome, rollouts, probes| DreamRunReport {
        subcommand,
        task,
        outcome,
        rollouts,
        probes,
        improved: false,
    };
    let usage = run(&["--bogus"]);
    assert_eq!((usage.exit_code, usage.report), (1, None));
    let empty = run(&["status", "--dir", &store]);
    assert_eq!(
        (empty.exit_code, empty.report),
        (
            2,
            Some(report(
                DreamSubcommand::Status,
                DreamTaskId::CirclePacking,
                DreamRunOutcome::Failed,
                0,
                0
            ))
        )
    );
    let llm = run(&["--llm-dreamer", "--task", "sum-difference"]);
    assert_eq!(
        (llm.exit_code, llm.report),
        (
            2,
            Some(report(
                DreamSubcommand::Loop,
                DreamTaskId::SumDifference,
                DreamRunOutcome::Unavailable,
                0,
                0
            ))
        )
    );
    let rollout = run(&[
        "rollout",
        "--task",
        "sum-difference",
        "--workers",
        "2",
        "--k1",
        "3",
        "--dir",
        &store,
    ]);
    let probes = rollout.report.map(|report| report.probes).expect("report");
    assert!(probes > 0);
    assert_eq!(
        (rollout.exit_code, rollout.report),
        (
            0,
            Some(report(
                DreamSubcommand::Rollout,
                DreamTaskId::SumDifference,
                DreamRunOutcome::Completed,
                1,
                probes
            ))
        )
    );
}
