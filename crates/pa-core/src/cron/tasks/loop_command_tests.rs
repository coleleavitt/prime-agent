use super::*;

fn fixed(cron: &str, rounded_to: Option<&str>, typed: &str, prompt: &str) -> LoopPlan {
    LoopPlan::Fixed {
        interval: LoopInterval {
            typed: typed.to_string(),
            cron: cron.to_string(),
            rounded_to: rounded_to.map(str::to_string),
        },
        prompt: prompt.to_string(),
    }
}

#[test]
fn the_parse_follows_claude_codes_rules() {
    let cases = [
        ("", Ok(LoopPlan::AutonomousDynamic)),
        ("   ", Ok(LoopPlan::AutonomousDynamic)),
        ("stop", Ok(LoopPlan::Stop)),
        ("STOP", Ok(LoopPlan::Stop)),
        // Rule 1: a leading interval token.
        (
            "5m /babysit-prs",
            Ok(fixed("*/5 * * * *", None, "5m", "/babysit-prs")),
        ),
        (
            "2h check the deploy",
            Ok(fixed("0 */2 * * *", None, "2h", "check the deploy")),
        ),
        (
            "1d summarize",
            Ok(fixed("0 0 * * *", None, "1d", "summarize")),
        ),
        ("1M ping", Ok(fixed("* * * * *", None, "1m", "ping"))),
        // Rule 2: a trailing "every" clause.
        (
            "check the deploy every 20m",
            Ok(fixed("*/20 * * * *", None, "20m", "check the deploy")),
        ),
        (
            "run tests every 5 minutes",
            Ok(fixed("*/5 * * * *", None, "5m", "run tests")),
        ),
        (
            "poll every 2 hours",
            Ok(fixed("0 */2 * * *", None, "2h", "poll")),
        ),
        // Rule 3: "every" without a time expression is part of the prompt.
        (
            "check every PR",
            Ok(LoopPlan::Dynamic {
                prompt: "check every PR".to_string(),
            }),
        ),
        (
            "check the deploy",
            Ok(LoopPlan::Dynamic {
                prompt: "check the deploy".to_string(),
            }),
        ),
        (
            "everyday 5m check",
            Ok(LoopPlan::Dynamic {
                prompt: "everyday 5m check".to_string(),
            }),
        ),
        // An interval and no prompt: the autonomous default on a cron.
        (
            "10m",
            Ok(LoopPlan::AutonomousFixed {
                interval: LoopInterval {
                    typed: "10m".to_string(),
                    cron: "*/10 * * * *".to_string(),
                    rounded_to: None,
                },
            }),
        ),
        (
            "every 2 hours",
            Ok(LoopPlan::AutonomousFixed {
                interval: LoopInterval {
                    typed: "2h".to_string(),
                    cron: "0 */2 * * *".to_string(),
                    rounded_to: None,
                },
            }),
        ),
        (
            "0m ping",
            Err("The loop interval must be at least 1.".to_string()),
        ),
        (
            "29d ping",
            Err("A day interval must be 1-28 days.".to_string()),
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(parse_loop(input), expected, "{input:?}");
    }
}

/// The interval table: minutes that divide an hour, hours that divide a
/// day, days at midnight; everything else rounds to the nearest clean
/// interval and says so.
#[test]
fn intervals_round_to_a_clean_cron() {
    let cases = [
        ("1m", "* * * * *", None),
        ("5m", "*/5 * * * *", None),
        ("7m", "*/6 * * * *", Some("6m")),
        ("45m", "*/30 * * * *", Some("30m")),
        ("60m", "0 * * * *", None),
        ("90m", "0 */2 * * *", Some("2h")),
        ("120m", "0 */2 * * *", None),
        ("5h", "0 */4 * * *", Some("4h")),
        ("6h", "0 */6 * * *", None),
        ("24h", "0 0 * * *", None),
        ("48h", "0 0 */2 * *", None),
        ("3d", "0 0 */3 * *", None),
        ("28d", "0 0 */28 * *", None),
        ("30s", "* * * * *", Some("1m")),
        ("60s", "* * * * *", None),
        ("90s", "*/2 * * * *", Some("2m")),
    ];
    for (typed, cron, rounded_to) in cases {
        let interval = interval_cron(typed).unwrap();
        assert_eq!(
            (interval.cron.as_str(), interval.rounded_to.as_deref()),
            (cron, rounded_to),
            "{typed}"
        );
    }
}
