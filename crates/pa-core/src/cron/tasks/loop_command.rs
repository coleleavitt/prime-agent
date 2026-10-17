//! `/loop [interval] <prompt>` (Claude Code's loop skill), parsed on the
//! host: a leading `5m`-style token or a trailing `every 20 minutes` clause
//! is a fixed interval (a recurring job); no interval means the model paces
//! itself with `rlm.schedule_wakeup`; no prompt means the autonomous
//! default; `stop` ends every loop.
//!
//! Claude Code asks the model to convert the interval; the host does it
//! here, with the same table: minutes that divide an hour, hours that
//! divide a day, days at midnight, seconds rounded up to a minute, and an
//! interval that does not divide its unit rounded to the nearest one that
//! does (the confirmation says so).

/// The parsed command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopPlan {
    /// `/loop stop`.
    Stop,
    /// A fixed interval with a prompt.
    Fixed {
        interval: LoopInterval,
        prompt: String,
    },
    /// A fixed interval and no prompt: the autonomous default on a cron.
    AutonomousFixed { interval: LoopInterval },
    /// A prompt and no interval: the model paces itself.
    Dynamic { prompt: String },
    /// No input at all: the autonomous default, self-paced.
    AutonomousDynamic,
}

/// An interval converted to a cron expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopInterval {
    /// What the user typed (`5m`, `every 20 minutes` reads `20m`).
    pub typed: String,
    pub cron: String,
    /// The interval actually scheduled, when it was rounded (`5m`).
    pub rounded_to: Option<String>,
}

/// The usage line.
pub const LOOP_USAGE: &str = "Usage: /loop [interval] <prompt> (interval: 5m, 2h, 1d, or a trailing \"every 20 minutes\"; no interval lets the model pace itself), /loop stop";

/// Parse the command's arguments.
///
/// # Errors
///
/// An interval the table cannot express (a zero or an over-long day count).
pub fn parse_loop(args: &str) -> Result<LoopPlan, String> {
    let input = args.trim();
    if input.is_empty() {
        return Ok(LoopPlan::AutonomousDynamic);
    }
    if input.eq_ignore_ascii_case("stop") {
        return Ok(LoopPlan::Stop);
    }
    let (typed, prompt) = match split_leading_interval(input) {
        Some(split) => split,
        None => match split_trailing_every(input) {
            Some(split) => split,
            None => {
                return Ok(LoopPlan::Dynamic {
                    prompt: input.to_string(),
                });
            }
        },
    };
    let interval = interval_cron(&typed)?;
    if prompt.is_empty() {
        return Ok(LoopPlan::AutonomousFixed { interval });
    }
    Ok(LoopPlan::Fixed { interval, prompt })
}

/// `5m rest`: the first token is `^\d+[smhd]$`.
fn split_leading_interval(input: &str) -> Option<(String, String)> {
    let (first, rest) = match input.split_once(char::is_whitespace) {
        Some((first, rest)) => (first, rest.trim()),
        None => (input, ""),
    };
    is_interval_token(first).then(|| (first.to_ascii_lowercase(), rest.to_string()))
}

fn is_interval_token(token: &str) -> bool {
    let Some(unit) = token.chars().last() else {
        return false;
    };
    let digits = &token[..token.len() - unit.len_utf8()];
    matches!(unit.to_ascii_lowercase(), 's' | 'm' | 'h' | 'd')
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// `check the deploy every 20m` / `every 5 minutes`: the trailing clause is
/// the interval only when a time expression follows `every`.
fn split_trailing_every(input: &str) -> Option<(String, String)> {
    let lower = input.to_ascii_lowercase();
    let at = lower.rfind("every")?;
    if at > 0 && !lower[..at].ends_with(char::is_whitespace) {
        return None;
    }
    let clause = lower[at + "every".len()..].trim();
    if lower[at + "every".len()..]
        .chars()
        .next()
        .is_some_and(|ch| !ch.is_whitespace())
    {
        return None;
    }
    let digits_end = clause
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(clause.len());
    if digits_end == 0 {
        return None;
    }
    let count = &clause[..digits_end];
    let unit = clause[digits_end..].trim();
    let unit = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 's',
        "m" | "min" | "mins" | "minute" | "minutes" => 'm',
        "h" | "hr" | "hrs" | "hour" | "hours" => 'h',
        "d" | "day" | "days" => 'd',
        _ => return None,
    };
    let prompt = input[..at].trim().to_string();
    Some((format!("{count}{unit}"), prompt))
}

/// The divisors of an hour and of a day: the intervals a `*/N` field
/// repeats evenly.
const MINUTE_STEPS: [u64; 11] = [1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30];
const HOUR_STEPS: [u64; 7] = [1, 2, 3, 4, 6, 8, 12];

fn nearest(value: u64, steps: &[u64]) -> u64 {
    steps
        .iter()
        .copied()
        .min_by_key(|step| (step.abs_diff(value), *step))
        .unwrap_or(1)
}

/// Convert an interval token (`5m`) to its cron expression.
///
/// # Errors
///
/// A zero interval, or more than 28 days.
pub fn interval_cron(typed: &str) -> Result<LoopInterval, String> {
    let unit = typed.chars().last().unwrap_or('m').to_ascii_lowercase();
    let count: u64 = typed[..typed.len() - unit.len_utf8()]
        .parse()
        .map_err(|_| LOOP_USAGE.to_string())?;
    if count == 0 {
        return Err("The loop interval must be at least 1.".to_string());
    }
    if unit == 'd' && count > 28 {
        return Err("A day interval must be 1-28 days.".to_string());
    }
    let requested_seconds = count.saturating_mul(match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        _ => 86_400,
    });
    // Seconds round up to whole minutes (cron's granularity).
    let minutes = requested_seconds.div_ceil(60);
    let (cron, scheduled, scheduled_seconds) = if minutes < 60 {
        let step = nearest(minutes, &MINUTE_STEPS);
        let cron = if step == 1 {
            "* * * * *".to_string()
        } else {
            format!("*/{step} * * * *")
        };
        (cron, format!("{step}m"), step * 60)
    } else if minutes < 24 * 60 {
        let hours = nearest(minutes.saturating_add(30) / 60, &HOUR_STEPS);
        let cron = if hours == 1 {
            "0 * * * *".to_string()
        } else {
            format!("0 */{hours} * * *")
        };
        (cron, format!("{hours}h"), hours * 3600)
    } else {
        let days = (minutes.saturating_add(720) / 1440).clamp(1, 28);
        let cron = if days == 1 {
            "0 0 * * *".to_string()
        } else {
            format!("0 0 */{days} * *")
        };
        (cron, format!("{days}d"), days * 86_400)
    };
    Ok(LoopInterval {
        typed: typed.to_string(),
        cron,
        rounded_to: (scheduled_seconds != requested_seconds).then_some(scheduled),
    })
}

#[cfg(test)]
#[path = "loop_command_tests.rs"]
mod tests;
