//! The `/ravo` session command's text surface (TS `slash-commands.ts`'s
//! `parseRavoCommandOptions`, `agent-session.ts`'s `ravo` case and
//! `_reportRavoRunCompletion`) and the agents-view status line (TS
//! `agents-view-state.ts`'s `formatRavoRunStatusLine`).

use serde_json::Value;

/// The session command.
pub const RAVO_COMMAND: &str = "ravo";
/// Its registry description (TS `BUILTIN_SLASH_COMMANDS`).
pub const RAVO_COMMAND_DESCRIPTION: &str = "Run the full RAVO loop (inspect, plan, implement, evaluate, diagnose, repair) over a continual harness mutation for a task";
/// Its argument hint.
pub const RAVO_COMMAND_HINT: &str =
    "[--global] [--rounds N] [--repairs N] [--arc-repo DIR --arc-game ID] <task>";
/// The usage error every malformed `/ravo` reports.
pub const RAVO_USAGE: &str =
    "Usage: /ravo [--global] [--rounds N] [--repairs N] [--arc-repo DIR --arc-game ID] <task>";

/// `/ravo`'s parsed arguments (TS `RavoCommandOptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RavoCommand {
    /// The non-flag tokens joined by single spaces.
    pub task: String,
    pub global: bool,
    pub max_rounds: Option<u64>,
    pub max_repairs: Option<u64>,
    /// `--arc-repo` and `--arc-game` (both or neither).
    pub arc_agi: Option<ArcAgiTarget>,
}

/// The ARC-AGI evaluator a `/ravo` named (TS `{ kind: "arc-agi", repoDir, game }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcAgiTarget {
    pub repo_dir: String,
    pub game: String,
}

/// JS `String.prototype.trim`'s set: `White_Space` plus the BOM.
fn js_trimmed(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// The TS tokenizer's separators, `[\t\p{Zs} ]`: tab and the space
/// separators, not line terminators (a task may span lines).
fn separator(c: char) -> bool {
    c.is_whitespace()
        && !matches!(
            c,
            '\n' | '\u{b}' | '\u{c}' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}'
        )
}

/// What JS `.` does not match: an inline value spanning a line terminator
/// fails the TS flag pattern, so the token is task text.
fn single_line(text: &str) -> bool {
    !text.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
}

/// `--flag` or `--flag=value` for one of `names` (TS `/^--(a|b)(?:=(.*))?$/`).
fn flag<'a>(token: &'a str, names: &[&'static str]) -> Option<(&'static str, Option<&'a str>)> {
    let rest = token.strip_prefix("--")?;
    names.iter().find_map(|name| {
        let tail = rest.strip_prefix(name)?;
        if tail.is_empty() {
            Some((*name, None))
        } else {
            tail.strip_prefix('=')
                .filter(|value| single_line(value))
                .map(|value| (*name, Some(value)))
        }
    })
}

/// TS `parseRavoCount`: ASCII digits, at least 1. A count past `u64` is
/// clamped (TS kept the double).
fn count(flag: &str, value: Option<&str>) -> Result<u64, String> {
    let error = || format!("{RAVO_USAGE} ({flag} expects a positive integer)");
    let digits = value
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(error)?;
    let number = digits.parse::<u64>().unwrap_or(u64::MAX);
    if number < 1 {
        return Err(error());
    }
    Ok(number)
}

/// Parse `/ravo` arguments (TS `parseRavoCommandOptions`): flags may appear
/// anywhere; the remaining tokens, joined by single spaces, are the task.
///
/// # Errors
///
/// [`RAVO_USAGE`] (with the flag for a bad count) for a missing task, a
/// flag without its value, or only one of `--arc-repo` / `--arc-game`.
pub fn parse_ravo_command(args: &str) -> Result<RavoCommand, String> {
    let tokens: Vec<&str> = args
        .trim_matches(js_trimmed)
        .split(separator)
        .filter(|token| !token.is_empty())
        .collect();
    let mut task_tokens: Vec<&str> = Vec::new();
    let mut global = false;
    let (mut max_rounds, mut max_repairs) = (None, None);
    let (mut arc_repo, mut arc_game): (Option<String>, Option<String>) = (None, None);
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if token == "--global" {
            global = true;
        } else if let Some((name, inline)) = flag(token, &["arc-repo", "arc-game"]) {
            let value = if inline.is_some() {
                inline
            } else {
                index += 1;
                tokens.get(index).copied()
            };
            let value = value
                .filter(|value| !value.is_empty())
                .ok_or_else(|| RAVO_USAGE.to_string())?;
            if name == "arc-repo" {
                arc_repo = Some(value.to_string());
            } else {
                arc_game = Some(value.to_string());
            }
        } else if let Some((name, inline)) = flag(token, &["rounds", "repairs"]) {
            let value = if inline.is_some() {
                inline
            } else {
                index += 1;
                tokens.get(index).copied()
            };
            let number = count(&format!("--{name}"), value)?;
            if name == "rounds" {
                max_rounds = Some(number);
            } else {
                max_repairs = Some(number);
            }
        } else {
            task_tokens.push(token);
        }
        index += 1;
    }
    let task = task_tokens.join(" ");
    if task.is_empty() || arc_repo.is_some() != arc_game.is_some() {
        return Err(RAVO_USAGE.to_string());
    }
    Ok(RavoCommand {
        task,
        global,
        max_rounds,
        max_repairs,
        arc_agi: arc_repo
            .zip(arc_game)
            .map(|(repo_dir, game)| ArcAgiTarget { repo_dir, game }),
    })
}

/// JS truthiness of a status field.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(_) => true,
    }
}

/// A status field as a JS template literal renders it.
fn js_text(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) if number.is_u64() || number.is_i64() => number.to_string(),
        Some(Value::Number(number)) => crate::js::js_number(number.as_f64().unwrap_or(0.0)),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_text(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

/// One-line RAVO status for the agents view (TS `formatRavoRunStatusLine`):
/// phase and counters while running, the stop reason once finished.
#[must_use]
pub fn ravo_status_line(status: &Value) -> String {
    if truthy(status.get("stopReason")) {
        return format!("ravo {}", js_text(status.get("stopReason")));
    }
    if truthy(status.get("error")) {
        return "ravo error".to_string();
    }
    let mut parts = vec![format!(
        "ravo {} r{}/{}",
        js_text(status.get("phase")),
        js_text(status.get("round")),
        js_text(status.get("repairs"))
    )];
    let certificate = status.get("lastCertificate").filter(|c| truthy(Some(c)));
    if let Some(certificate) = certificate {
        let score = certificate
            .get("deepScore")
            .filter(|score| !score.is_null())
            .or_else(|| certificate.get("screenScore"));
        parts.push(format!(
            "{} {}",
            js_text(certificate.get("status")),
            js_text(score)
        ));
        if let Some(missed) = certificate
            .get("missed")
            .and_then(Value::as_array)
            .filter(|missed| !missed.is_empty())
        {
            parts.push(format!(
                "missed: {}",
                js_text(Some(&Value::from(missed.clone())))
            ));
        }
    }
    parts.join(" · ")
}

/// The `/ravo` started row (TS `RAVO run <id> started: <task>`).
#[must_use]
pub fn started_text(run_id: &str, task: &str) -> String {
    format!("RAVO run {run_id} started: {task}")
}

/// The durable terminal row of a `/ravo` run (TS `_reportRavoRunCompletion`):
/// `Ok` the stop reason, `Err` the failure (the session prefixes
/// `Command failed: `). `None` is a run that ended without a status.
///
/// # Errors
///
/// `RAVO run <id> failed: <error>` when the run failed.
pub fn completion_text(run_id: &str, status: Option<&Value>) -> Result<String, String> {
    let Some(status) = status.filter(|status| status.is_object()) else {
        return Err(format!(
            "RAVO run {run_id} failed: the RAVO run ended without a status"
        ));
    };
    // TS rejected the completion with the run's error (any string).
    if let Some(error) = status.get("error").and_then(Value::as_str) {
        return Err(format!("RAVO run {run_id} failed: {error}"));
    }
    let reason = status
        .get("stopReason")
        .filter(|reason| !reason.is_null())
        .map_or_else(|| "stopped".to_string(), |reason| js_text(Some(reason)));
    Ok(format!("RAVO run {run_id} {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parsed(args: &str) -> Result<RavoCommand, String> {
        parse_ravo_command(args)
    }

    fn command(task: &str) -> RavoCommand {
        RavoCommand {
            task: task.to_string(),
            global: false,
            max_rounds: None,
            max_repairs: None,
            arc_agi: None,
        }
    }

    #[test]
    fn flags_may_appear_anywhere_and_the_rest_is_the_task() {
        assert_eq!(
            parsed("  note the --global  tactic --rounds 3 --repairs=2 "),
            Ok(RavoCommand {
                global: true,
                max_rounds: Some(3),
                max_repairs: Some(2),
                ..command("note the tactic")
            })
        );
        assert_eq!(
            parsed("play --arc-repo=/r --arc-game ls20"),
            Ok(RavoCommand {
                arc_agi: Some(ArcAgiTarget {
                    repo_dir: "/r".to_string(),
                    game: "ls20".to_string()
                }),
                ..command("play")
            })
        );
        // Tabs and space separators split; line breaks stay in the task.
        assert_eq!(parsed("a\tb\u{a0}c"), Ok(command("a b c")));
        assert_eq!(parsed("a\nb c"), Ok(command("a\nb c")));
        // Not a flag the parser knows: task text.
        assert_eq!(
            parsed("--global=yes --roundsx 2"),
            Ok(command("--global=yes --roundsx 2"))
        );
        assert_eq!(
            parsed("x --rounds 007"),
            Ok(RavoCommand {
                max_rounds: Some(7),
                ..command("x")
            })
        );
    }

    #[test]
    fn malformed_arguments_report_the_ts_usage() {
        for args in [
            "",
            "   ",
            "--global",
            "x --arc-repo /r",
            "x --arc-game ls20",
            "x --arc-repo",
            "x --arc-repo= --arc-game g",
        ] {
            assert_eq!(parsed(args), Err(RAVO_USAGE.to_string()), "{args:?}");
        }
        for (args, flag) in [
            ("x --rounds 0", "--rounds"),
            ("x --rounds", "--rounds"),
            ("x --repairs=-1", "--repairs"),
            ("x --repairs 1.5", "--repairs"),
            ("x --rounds=", "--rounds"),
        ] {
            assert_eq!(
                parsed(args),
                Err(format!("{RAVO_USAGE} ({flag} expects a positive integer)")),
                "{args:?}"
            );
        }
    }

    /// The TS `agents-view-state.test.ts` cases.
    #[test]
    fn the_status_line_matches_the_ts_formatter() {
        let base = json!({ "runId": "run-1", "phase": "evaluate", "round": 2, "repairs": 1, "startedAt": 1, "updatedAt": 2 });
        let with = |patch: Value| {
            let mut status = base.clone();
            for (key, value) in patch.as_object().unwrap() {
                status[key] = value.clone();
            }
            status
        };
        assert_eq!(ravo_status_line(&base), "ravo evaluate r2/1");
        assert_eq!(
            ravo_status_line(&with(json!({ "lastCertificate": {
                "proposalId": "p-1", "status": "reject_deep", "screenScore": 90, "deepScore": 41,
                "missed": ["evidence", "scope"]
            } }))),
            "ravo evaluate r2/1 · reject_deep 41 · missed: evidence,scope"
        );
        assert_eq!(
            ravo_status_line(&with(json!({ "lastCertificate": {
                "proposalId": "p-1", "status": "reject_screen", "screenScore": 12, "missed": []
            } }))),
            "ravo evaluate r2/1 · reject_screen 12"
        );
        assert_eq!(
            ravo_status_line(&with(
                json!({ "phase": "stopped", "stopReason": "round_limit" })
            )),
            "ravo round_limit"
        );
        assert_eq!(
            ravo_status_line(&with(json!({ "phase": "stopped", "error": "boom" }))),
            "ravo error"
        );
        assert_eq!(
            ravo_status_line(&with(json!({ "lastCertificate": {
                "status": "commit", "screenScore": 80, "deepScore": 82.5, "missed": []
            } }))),
            "ravo evaluate r2/1 · commit 82.5"
        );
    }

    #[test]
    fn the_terminal_row_names_the_run_and_how_it_ended() {
        assert_eq!(
            completion_text(
                "ravo_1",
                Some(&json!({ "phase": "accepted", "stopReason": "accepted" }))
            ),
            Ok("RAVO run ravo_1 accepted".to_string())
        );
        assert_eq!(
            completion_text("ravo_1", Some(&json!({ "phase": "stopped" }))),
            Ok("RAVO run ravo_1 stopped".to_string())
        );
        assert_eq!(
            completion_text(
                "ravo_1",
                Some(&json!({ "phase": "stopped", "error": "disk full" }))
            ),
            Err("RAVO run ravo_1 failed: disk full".to_string())
        );
        assert_eq!(
            completion_text("ravo_1", None),
            Err("RAVO run ravo_1 failed: the RAVO run ended without a status".to_string())
        );
        assert_eq!(
            started_text("ravo_1", "note it"),
            "RAVO run ravo_1 started: note it"
        );
    }
}
