//! Request parsing for the in-session surfaces (TS `agent-session.ts`
//! `parseDreamRunPayload` / `parseDreamExperimentPayload` and
//! `slash-commands.ts` `parseDreamCommandOptions`): the kernel skill's
//! `dream.run` / `dream.experiment` payloads and the `/dream` arguments.
//! Every error message is the TS one.

use serde_json::{Map, Value};

use crate::experiment::ExperimentArm;
use crate::policy::PRIMING_DIVERSE;
use crate::run_service::{
    DreamChildOptions, DreamExperimentRequest, DreamRunRequest, DREAM_MAX_SEEDS,
};
use crate::tasks::{DreamTaskId, DREAM_TASK_IDS};

/// The thinking levels a child may run at (TS `THINKING_LEVELS`).
pub const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

fn task_names() -> String {
    DREAM_TASK_IDS
        .iter()
        .map(|task| task.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn present<'a>(payload: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    payload.get(key).filter(|value| !value.is_null())
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|x| x.fract() == 0.0 && x.abs() < 9e15)
            .map(|x| {
                #[allow(clippy::cast_possible_truncation)] // integral and within 2^53
                let integral = x as i64;
                integral
            })
    })
}

fn positive(payload: &Map<String, Value>, key: &str, request: &str) -> Result<Option<u64>, String> {
    let Some(value) = present(payload, key) else {
        return Ok(None);
    };
    match integer(value).filter(|value| *value >= 1) {
        Some(value) => Ok(Some(value.unsigned_abs())),
        None => Err(format!(
            "{request} {key} must be a positive integer when provided"
        )),
    }
}

fn positive_u32(
    payload: &Map<String, Value>,
    key: &str,
    request: &str,
) -> Result<Option<u32>, String> {
    positive(payload, key, request)?
        .map(|value| u32::try_from(value).map_err(|_| format!("{request} {key} is too large")))
        .transpose()
}

fn non_negative(
    payload: &Map<String, Value>,
    key: &str,
    request: &str,
) -> Result<Option<u64>, String> {
    let Some(value) = present(payload, key) else {
        return Ok(None);
    };
    match integer(value).filter(|value| *value >= 0) {
        Some(value) => Ok(Some(value.unsigned_abs())),
        None => Err(format!(
            "{request} {key} must be a non-negative integer when provided"
        )),
    }
}

fn boolean(payload: &Map<String, Value>, key: &str, request: &str) -> Result<bool, String> {
    match present(payload, key) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("{request} {key} must be a boolean when provided")),
    }
}

fn task(payload: &Map<String, Value>, request: &str) -> Result<DreamTaskId, String> {
    payload
        .get("task")
        .and_then(Value::as_str)
        .and_then(DreamTaskId::from_name)
        .ok_or_else(|| format!("{request} task must be one of {}", task_names()))
}

fn priming(
    payload: &Map<String, Value>,
    request: &str,
) -> Result<Vec<crate::policy::ExplorationPolicy>, String> {
    match present(payload, "priming") {
        None => Ok(Vec::new()),
        Some(Value::String(choice)) if choice == "none" => Ok(Vec::new()),
        Some(Value::String(choice)) if choice == "diverse" => Ok(PRIMING_DIVERSE.to_vec()),
        Some(_) => Err(format!(
            "{request} priming must be \"none\" or \"diverse\" when provided"
        )),
    }
}

fn child_options(payload: &Map<String, Value>, request: &str) -> Result<DreamChildOptions, String> {
    let model = match payload.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) => {
            let model = model.trim();
            if model.is_empty() {
                return Err(format!("{request} model must not be empty"));
            }
            Some(model.to_string())
        }
        Some(_) => return Err(format!("{request} model must be a string")),
    };
    let thinking = match payload.get("thinking") {
        None | Some(Value::Null) => None,
        Some(Value::String(level)) => {
            let level = level.trim().to_lowercase();
            if !THINKING_LEVELS.contains(&level.as_str()) {
                return Err(format!(
                    "{request} thinking must be one of: {}",
                    THINKING_LEVELS.join(", ")
                ));
            }
            Some(level)
        }
        Some(_) => return Err(format!("{request} thinking must be a string")),
    };
    Ok(DreamChildOptions {
        model,
        thinking,
        max_output_tokens: positive(payload, "max_output_tokens", request)?,
    })
}

/// Parse a `dream.run` payload.
///
/// # Errors
///
/// The TS validation message.
pub fn parse_run_payload(payload: &Map<String, Value>) -> Result<DreamRunRequest, String> {
    let request = "dream.run";
    Ok(DreamRunRequest {
        task: task(payload, request)?,
        n: positive(payload, "n", request)?.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        seed: non_negative(payload, "seed", request)?,
        workers: positive_u32(payload, "workers", request)?,
        k1: positive_u32(payload, "k1", request)?,
        k2: positive_u32(payload, "k2", request)?,
        dreams: positive_u32(payload, "dreams", request)?,
        iterations: positive_u32(payload, "iterations", request)?,
        llm_proposer: boolean(payload, "llm_proposer", request)?,
        llm_dreamer: boolean(payload, "llm_dreamer", request)?,
        child: child_options(payload, request)?,
        priming_policies: priming(payload, request)?,
    })
}

const ARM_NAMES: &str = "dream, fixed, dream-guided, fixed-guided";

/// Parse a `dream.experiment` payload.
///
/// # Errors
///
/// The TS validation message.
pub fn parse_experiment_payload(
    payload: &Map<String, Value>,
) -> Result<DreamExperimentRequest, String> {
    let request = "dream.experiment";
    let task = task(payload, request)?;
    let n = positive(payload, "n", request)?.map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let seed = non_negative(payload, "seed", request)?;
    let seeds = match present(payload, "seeds") {
        None => None,
        Some(value) => {
            let invalid = "dream.experiment seeds must be a non-empty array of distinct non-negative integers".to_string();
            let Some(items) = value.as_array().filter(|items| !items.is_empty()) else {
                return Err(invalid);
            };
            if items.len() > DREAM_MAX_SEEDS {
                return Err(format!(
                    "dream.experiment seeds must list at most {DREAM_MAX_SEEDS} seeds (got {})",
                    items.len()
                ));
            }
            let mut seeds = Vec::with_capacity(items.len());
            for item in items {
                let Some(seed) = integer(item)
                    .filter(|seed| *seed >= 0)
                    .map(i64::unsigned_abs)
                else {
                    return Err(invalid);
                };
                if seeds.contains(&seed) {
                    return Err(invalid);
                }
                seeds.push(seed);
            }
            Some(seeds)
        }
    };
    if seed.is_some() && seeds.is_some() {
        return Err(format!("{request} takes either seed or seeds, not both"));
    }
    let rounds = positive_u32(payload, "rounds", request)?;
    let arms = match present(payload, "arms") {
        None => None,
        Some(value) => {
            let Some(items) = value.as_array().filter(|items| !items.is_empty()) else {
                return Err(format!(
                    "dream.experiment arms must be a non-empty array of {ARM_NAMES}"
                ));
            };
            let mut arms = Vec::with_capacity(items.len());
            for item in items {
                let Some(arm) = item.as_str().and_then(ExperimentArm::from_name) else {
                    let shown = item
                        .as_str()
                        .map_or_else(|| item.to_string(), str::to_string);
                    return Err(format!(
                        "dream.experiment arms must be one of {ARM_NAMES} (got {shown})"
                    ));
                };
                if arms.contains(&arm) {
                    return Err(format!(
                        "dream.experiment arms must be distinct ({} repeats)",
                        arm.as_str()
                    ));
                }
                arms.push(arm);
            }
            Some(arms)
        }
    };
    let llm_proposer = boolean(payload, "llm_proposer", request)?;
    if arms
        .as_ref()
        .is_some_and(|arms| arms.iter().any(|arm| arm.guided()))
        && !llm_proposer
    {
        return Err("dream-guided/fixed-guided require llm_proposer".to_string());
    }
    Ok(DreamExperimentRequest {
        task,
        n,
        seed,
        seeds,
        rounds,
        arms,
        workers: positive_u32(payload, "workers", request)?,
        k1: positive_u32(payload, "k1", request)?,
        k2: positive_u32(payload, "k2", request)?,
        dreams: positive_u32(payload, "dreams", request)?,
        llm_proposer,
        llm_dreamer: boolean(payload, "llm_dreamer", request)?,
        child: child_options(payload, request)?,
        priming_policies: priming(payload, request)?,
    })
}

/// The `/dream` usage line (TS `DREAM_USAGE` of `slash-commands.ts`).
#[must_use]
pub fn dream_slash_usage() -> String {
    let tasks: Vec<&str> = DREAM_TASK_IDS.iter().map(|task| task.as_str()).collect();
    format!(
        "Usage: /dream [experiment] [--task <{}>] [--n N] [--seed N] [--seeds a,b,c] [--workers N] [--k1 N] [--k2 N] [--dreams N] [--iterations N] [--rounds N] [--arms dream,fixed] [--priming none|diverse] [--model provider/id] [--thinking <{}>] [--max-output-tokens N] [--llm-proposer] [--llm-dreamer]",
        tasks.join("|"),
        THINKING_LEVELS.join("|")
    )
}

/// The `/dream` value flags.
const FLAGS: &[&str] = &[
    "task",
    "n",
    "seed",
    "seeds",
    "workers",
    "k1",
    "k2",
    "dreams",
    "iterations",
    "rounds",
    "arms",
    "priming",
    "model",
    "thinking",
    "max-output-tokens",
];

/// What `/dream` asks for: a run, or an experiment.
#[derive(Debug, Clone, PartialEq)]
pub enum DreamCommand {
    Run(DreamRunRequest),
    Experiment(DreamExperimentRequest),
}

fn usage_with(detail: &str) -> String {
    format!("{} ({detail})", dream_slash_usage())
}

fn digits(value: Option<&str>) -> Option<u64> {
    value
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse().ok())
}

fn count(flag: &str, value: Option<&str>) -> Result<u64, String> {
    digits(value)
        .filter(|value| *value >= 1)
        .ok_or_else(|| usage_with(&format!("{flag} expects a positive integer")))
}

fn count_u32(flag: &str, value: Option<&str>) -> Result<u32, String> {
    u32::try_from(count(flag, value)?)
        .map_err(|_| usage_with(&format!("{flag} expects a positive integer")))
}

/// Parse `/dream` arguments (TS `parseDreamCommandOptions`): every knob is a
/// flag, the task defaults to circle-packing, a leading `experiment` selects
/// the controlled comparison; unknown flags and stray tokens are a usage error.
///
/// # Errors
///
/// The usage line, with the TS detail where the TS gives one.
#[allow(clippy::too_many_lines)] // one flag table, kept whole as in the TS
pub fn parse_dream_command(args: &str) -> Result<DreamCommand, String> {
    let tokens: Vec<&str> = args
        .split(|c: char| c.is_whitespace())
        .filter(|token| !token.is_empty())
        .collect();
    let mut task = DreamTaskId::CirclePacking;
    let (mut n, mut seed, mut workers, mut k1, mut k2, mut dreams) =
        (None, None, None, None, None, None);
    let (mut iterations, mut rounds, mut arms, mut seeds) = (None, None, None, None);
    let mut child = DreamChildOptions::default();
    let mut priming_policies = Vec::new();
    let (mut llm_proposer, mut llm_dreamer, mut experiment) = (false, false, false);
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if index == 0 && token == "experiment" {
            experiment = true;
            index += 1;
            continue;
        }
        if token == "--llm-proposer" {
            llm_proposer = true;
            index += 1;
            continue;
        }
        if token == "--llm-dreamer" {
            llm_dreamer = true;
            index += 1;
            continue;
        }
        let Some(flag_text) = token.strip_prefix("--") else {
            return Err(dream_slash_usage());
        };
        let (flag, inline) = match flag_text.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (flag_text, None),
        };

        if !FLAGS.contains(&flag) {
            return Err(dream_slash_usage());
        }
        let value = if inline.is_some() {
            inline
        } else {
            index += 1;
            tokens.get(index).copied()
        };
        match flag {
            "task" => {
                task = value
                    .and_then(DreamTaskId::from_name)
                    .ok_or_else(dream_slash_usage)?;
            }
            "n" => n = Some(usize::try_from(count("--n", value)?).unwrap_or(usize::MAX)),
            "seed" => {
                seed = Some(
                    digits(value)
                        .ok_or_else(|| usage_with("--seed expects a non-negative integer"))?,
                );
            }
            "workers" => workers = Some(count_u32("--workers", value)?),
            "k1" => k1 = Some(count_u32("--k1", value)?),
            "k2" => k2 = Some(count_u32("--k2", value)?),
            "dreams" => dreams = Some(count_u32("--dreams", value)?),
            "iterations" => iterations = Some(count_u32("--iterations", value)?),
            "rounds" => rounds = Some(count_u32("--rounds", value)?),
            "max-output-tokens" => {
                child.max_output_tokens = Some(count("--max-output-tokens", value)?);
            }
            "arms" => {
                let names: Vec<&str> = value
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .collect();
                if names.is_empty() {
                    return Err(usage_with("--arms expects a comma-separated list of arms"));
                }
                let mut list = Vec::new();
                for name in names {
                    match ExperimentArm::from_name(name) {
                        Some(arm) if !list.contains(&arm) => list.push(arm),
                        _ => {
                            return Err(usage_with(
                                "--arms expects distinct arms out of dream, fixed, dream-guided, fixed-guided",
                            ))
                        }
                    }
                }
                arms = Some(list);
            }
            "seeds" => {
                let parts: Vec<&str> = value
                    .unwrap_or_default()
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect();
                if parts.is_empty() {
                    return Err(usage_with(
                        "--seeds expects a comma-separated list of seeds",
                    ));
                }
                if parts.len() > DREAM_MAX_SEEDS {
                    return Err(usage_with(&format!(
                        "--seeds lists at most {DREAM_MAX_SEEDS} seeds"
                    )));
                }
                let mut list = Vec::new();
                for part in parts {
                    match digits(Some(part)) {
                        Some(seed) if !list.contains(&seed) => list.push(seed),
                        _ => {
                            return Err(usage_with(
                                "--seeds expects distinct non-negative integers",
                            ))
                        }
                    }
                }
                seeds = Some(list);
            }
            "priming" => match value {
                Some("none") => priming_policies = Vec::new(),
                Some("diverse") => priming_policies = PRIMING_DIVERSE.to_vec(),
                _ => return Err(usage_with("--priming expects none or diverse")),
            },
            "model" => {
                let model = value.map(str::trim).filter(|model| !model.is_empty());
                child.model = Some(
                    model
                        .ok_or_else(|| usage_with("--model expects a provider/id selector"))?
                        .to_string(),
                );
            }
            "thinking" => {
                let level = value.map(|level| level.trim().to_lowercase());
                match level {
                    Some(level) if THINKING_LEVELS.contains(&level.as_str()) => {
                        child.thinking = Some(level);
                    }
                    _ => {
                        return Err(usage_with(&format!(
                            "--thinking expects one of {}",
                            THINKING_LEVELS.join(", ")
                        )))
                    }
                }
            }
            _ => return Err(dream_slash_usage()),
        }
        index += 1;
    }
    if experiment && iterations.is_some() {
        return Err(usage_with("experiment takes --rounds, not --iterations"));
    }
    if !experiment && (rounds.is_some() || arms.is_some() || seeds.is_some()) {
        return Err(usage_with(
            "--rounds, --arms and --seeds belong to /dream experiment",
        ));
    }
    if seeds.is_some() && seed.is_some() {
        return Err(usage_with("--seed and --seeds are exclusive"));
    }
    if arms
        .as_ref()
        .is_some_and(|arms: &Vec<ExperimentArm>| arms.iter().any(|arm| arm.guided()))
        && !llm_proposer
    {
        return Err(usage_with(
            "dream-guided/fixed-guided require --llm-proposer",
        ));
    }
    if experiment {
        return Ok(DreamCommand::Experiment(DreamExperimentRequest {
            task,
            n,
            seed,
            seeds,
            rounds,
            arms,
            workers,
            k1,
            k2,
            dreams,
            llm_proposer,
            llm_dreamer,
            child,
            priming_policies,
        }));
    }
    Ok(DreamCommand::Run(DreamRunRequest {
        task,
        n,
        seed,
        workers,
        k1,
        k2,
        dreams,
        iterations,
        llm_proposer,
        llm_dreamer,
        child,
        priming_policies,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[allow(clippy::needless_pass_by_value)] // takes the json! literal
    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn a_run_payload_parses_every_knob_and_rejects_with_the_ts_messages() {
        let request = parse_run_payload(&object(json!({
            "task": "sum-difference", "n": null, "seed": 0, "workers": 2, "k1": 3, "k2": 6,
            "dreams": 4, "iterations": 2, "llm_proposer": true, "llm_dreamer": false,
            "model": " faux/child ", "thinking": "LOW", "max_output_tokens": 512, "priming": "diverse"
        })))
        .unwrap();
        assert_eq!(
            request,
            DreamRunRequest {
                task: DreamTaskId::SumDifference,
                n: None,
                seed: Some(0),
                workers: Some(2),
                k1: Some(3),
                k2: Some(6),
                dreams: Some(4),
                iterations: Some(2),
                llm_proposer: true,
                llm_dreamer: false,
                child: DreamChildOptions {
                    model: Some("faux/child".to_string()),
                    thinking: Some("low".to_string()),
                    max_output_tokens: Some(512),
                },
                priming_policies: PRIMING_DIVERSE.to_vec(),
            }
        );
        for (payload, message) in [
            (json!({"task": "nope"}), "dream.run task must be one of circle-packing, sum-difference, python-speedup, autocorrelation"),
            (json!({"task": "sum-difference", "k1": 0}), "dream.run k1 must be a positive integer when provided"),
            (json!({"task": "sum-difference", "seed": -1}), "dream.run seed must be a non-negative integer when provided"),
            (json!({"task": "sum-difference", "llm_dreamer": "yes"}), "dream.run llm_dreamer must be a boolean when provided"),
            (json!({"task": "sum-difference", "priming": "wide"}), "dream.run priming must be \"none\" or \"diverse\" when provided"),
            (json!({"task": "sum-difference", "thinking": "huge"}), "dream.run thinking must be one of: off, minimal, low, medium, high, xhigh, max"),
            (json!({"task": "sum-difference", "model": "  "}), "dream.run model must not be empty"),
        ] {
            assert_eq!(parse_run_payload(&object(payload)).err().as_deref(), Some(message));
        }
    }

    #[test]
    fn an_experiment_payload_validates_seeds_arms_and_guided_arms() {
        let request = parse_experiment_payload(&object(json!({
            "task": "autocorrelation", "seeds": [7, 8], "arms": ["dream", "fixed-guided"],
            "rounds": 3, "llm_proposer": true
        })))
        .unwrap();
        assert_eq!(request.seeds, Some(vec![7, 8]));
        assert_eq!(
            request.arms,
            Some(vec![ExperimentArm::Dream, ExperimentArm::FixedGuided])
        );
        assert_eq!(request.rounds, Some(3));
        for (payload, message) in [
            (json!({"task": "autocorrelation", "seed": 1, "seeds": [2]}), "dream.experiment takes either seed or seeds, not both".to_string()),
            (json!({"task": "autocorrelation", "seeds": [1, 1]}), "dream.experiment seeds must be a non-empty array of distinct non-negative integers".to_string()),
            (json!({"task": "autocorrelation", "seeds": (0..17).collect::<Vec<_>>()}), "dream.experiment seeds must list at most 16 seeds (got 17)".to_string()),
            (json!({"task": "autocorrelation", "arms": []}), format!("dream.experiment arms must be a non-empty array of {ARM_NAMES}")),
            (json!({"task": "autocorrelation", "arms": ["wild"]}), format!("dream.experiment arms must be one of {ARM_NAMES} (got wild)")),
            (json!({"task": "autocorrelation", "arms": ["dream", "dream"]}), "dream.experiment arms must be distinct (dream repeats)".to_string()),
            (json!({"task": "autocorrelation", "arms": ["dream-guided"]}), "dream-guided/fixed-guided require llm_proposer".to_string()),
        ] {
            assert_eq!(parse_experiment_payload(&object(payload)).err(), Some(message));
        }
    }

    #[test]
    fn the_slash_command_parses_runs_and_experiments_and_refuses_with_the_usage() {
        assert_eq!(
            parse_dream_command(""),
            Ok(DreamCommand::Run(DreamRunRequest::new(
                DreamTaskId::CirclePacking
            )))
        );
        let Ok(DreamCommand::Run(run)) =
            parse_dream_command("--task=sum-difference --seed 0 --iterations 2 --llm-dreamer --thinking HIGH --priming none")
        else {
            panic!("a run");
        };
        assert_eq!(
            (
                run.task,
                run.seed,
                run.iterations,
                run.llm_dreamer,
                run.child.thinking.as_deref()
            ),
            (
                DreamTaskId::SumDifference,
                Some(0),
                Some(2),
                true,
                Some("high")
            )
        );
        let Ok(DreamCommand::Experiment(experiment)) =
            parse_dream_command("experiment --task autocorrelation --seeds 7,8 --arms dream,dream-guided --llm-proposer --rounds 3")
        else {
            panic!("an experiment");
        };
        assert_eq!(experiment.seeds, Some(vec![7, 8]));
        assert_eq!(experiment.rounds, Some(3));
        let usage = dream_slash_usage();
        for (args, detail) in [
            ("--bogus", None),
            ("stray", None),
            ("--task nope", None),
            ("--k1 0", Some("--k1 expects a positive integer")),
            (
                "experiment --iterations 2",
                Some("experiment takes --rounds, not --iterations"),
            ),
            (
                "--rounds 2",
                Some("--rounds, --arms and --seeds belong to /dream experiment"),
            ),
            (
                "experiment --seed 1 --seeds 2",
                Some("--seed and --seeds are exclusive"),
            ),
            (
                "experiment --arms fixed-guided",
                Some("dream-guided/fixed-guided require --llm-proposer"),
            ),
            (
                "experiment --seeds 1,1",
                Some("--seeds expects distinct non-negative integers"),
            ),
            ("--model", Some("--model expects a provider/id selector")),
        ] {
            let expected =
                detail.map_or_else(|| usage.clone(), |detail| format!("{usage} ({detail})"));
            assert_eq!(parse_dream_command(args).err(), Some(expected), "{args}");
        }
        assert!(usage.starts_with("Usage: /dream [experiment] [--task <circle-packing|sum-difference|python-speedup|autocorrelation>]"));
    }
}
