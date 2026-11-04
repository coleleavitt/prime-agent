//! Foreground-sleep rules: block waits that hold the turn open for something
//! the session can be told about (`rlm.monitor` over an `until` loop, a
//! `bash()` handle's done notice). Three checks:
//!
//! 1. A command whose first command is exactly `sleep N` with `N` of 25
//!    seconds or more is refused, so `sleep 60 && make` becomes
//!    `rlm.monitor` or a plain `bash()` handle.
//! 2. A loop whose body polls with `sleep` is refused when it holds the
//!    turn open: budget (iterations * seconds) reaches [`BUDGET_SECONDS`]
//!    when both are literal, the per-iteration sleep reaches
//!    [`IN_LOOP_SLEEP_SECONDS`] when the iteration count is not literal, or
//!    the loop's condition is always true. `until`, a backgrounded loop,
//!    and the standalone `sleep N &` pass; those are the shapes the model
//!    is supposed to use.
//! 3. A top-level `watch` call is refused: `watch` repeats its command
//!    forever (default interval two seconds, `-n` / `--interval` sets the
//!    delay) and never exits on its own, so it holds the turn open the
//!    same way a `while :; do …; sleep S; done` would. `watch &` and
//!    `watch` from inside a subshell that is itself backgrounded pass.

use super::{Check, Rule};
use crate::syntax::ast::{BranchKind, Command, List, Part, Quoting, SimpleCommand, Word};
use crate::syntax::parse::parse;
use crate::verdict::GuardKind;

pub(crate) struct Sleep;

const LATE_BYPASS_WARNING: &str = "prime-agent bash: PI_BASH_ALLOW_SLEEP appeared after kernel start and is ignored; the sleep guard only honors it when the kernel is started with it set.";
const REFUSED_FROM_SECONDS: f64 = 25.0;
/// Combined `iterations * sleep_seconds` that trips the polling-loop check.
/// Picked just under a minute: the smallest polling loop seen refused in the
/// session corpus spent 60 seconds inside `sleep`.
const BUDGET_SECONDS: f64 = 60.0;
/// Per-iteration sleep that trips the polling-loop check when the iteration
/// count is not literal: at this point the loop holds the turn as long as
/// a leading `sleep 10` would.
const IN_LOOP_SLEEP_SECONDS: f64 = 10.0;

impl Rule for Sleep {
    const GUARD: GuardKind = GuardKind::Sleep;
    const LATE_BYPASS_WARNING: Option<&'static str> = Some(LATE_BYPASS_WARNING);

    fn judge(check: &Check<'_>) -> Option<String> {
        let list = parse(check.script.command).list;
        if let Some(what) = leading_sleep(&list) {
            return Some(refusal_message(&what));
        }
        if let Some(what) = polling_loop(&list) {
            return Some(refusal_message(&what));
        }
        if let Some(what) = top_level_watch(&list) {
            return Some(refusal_message(&what));
        }
        None
    }
}

fn refusal_message(what: &str) -> String {
    format!(
        "Refusing to run this command: {what}. A foreground wait like this holds the turn open for a wait you can be notified about. To wait for a condition, run rlm.monitor over an until-loop (e.g. rlm.monitor(\"until <check>; do sleep 2; done\")): you get a notice when the loop exits. To wait for a command you started, keep its bash() handle and end the turn: its done notice arrives on its own. Do not chain shorter sleeps or shrink iteration budgets to work around this. Bypass deliberately, so the intent stays visible in the transcript: call bash(command, allow_sleep=True), or start the kernel with PI_BASH_ALLOW_SLEEP=1."
    )
}

fn leading_sleep(list: &List) -> Option<String> {
    let first = list.items.first()?;
    if first.background || first.first.negated {
        return None;
    }
    let Command::Simple(command) = first.first.commands.first()? else {
        return None;
    };
    let [program, amount] = command.words.as_slice() else {
        return None;
    };
    if program.raw != "sleep" || !command.assignments.is_empty() {
        return None;
    }
    let seconds = parse_raw_sleep_number(&amount.raw)?;
    if seconds < REFUSED_FROM_SECONDS {
        return None;
    }
    let amount = &amount.raw;
    let shape = if list.items.len() > 1 || !first.rest.is_empty() || first.first.commands.len() > 1
    {
        format!("sleep {amount} followed by other commands")
    } else {
        format!("standalone sleep {amount}")
    };
    Some(shape)
}

/// `amount` as a number when it is spelled as plain ASCII digits (no quoting,
/// no expansion, no unit): what the old leading-sleep rule reads, so the
/// policy on quoted numbers like `sleep '30'` is unchanged.
fn parse_raw_sleep_number(raw: &str) -> Option<f64> {
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    raw.parse().ok()
}

fn polling_loop(list: &List) -> Option<String> {
    let mut found = Vec::new();
    collect_loops(list, false, &mut found);
    found.iter().find_map(describe_offending_loop)
}

#[derive(Debug)]
struct LoopInfo {
    kind: LoopKind,
    iters: Option<u64>,
    sleep_s: Option<f64>,
    always_true: bool,
}

#[derive(Debug, Clone, Copy)]
enum LoopKind {
    For,
    While,
}

impl LoopKind {
    fn label(self) -> &'static str {
        match self {
            LoopKind::For => "for",
            LoopKind::While => "while",
        }
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "iters is a shell iteration count, always well under 2^53; the f64 product tracks a human-readable wall-clock budget"
)]
fn describe_offending_loop(info: &LoopInfo) -> Option<String> {
    let sleep_s = info.sleep_s?;
    let kind = info.kind.label();
    if let Some(iters) = info.iters {
        let budget = sleep_s * (iters as f64);
        if budget >= BUDGET_SECONDS {
            return Some(format!(
                "{kind} loop with {iters} iterations sleeping {sleep_s} seconds each ({budget} seconds total)",
            ));
        }
        return None;
    }
    if info.always_true {
        return Some(format!(
            "{kind} loop with an always-true condition sleeping {sleep_s} seconds each iteration",
        ));
    }
    if sleep_s >= IN_LOOP_SLEEP_SECONDS {
        return Some(format!(
            "{kind} loop sleeping {sleep_s} seconds each iteration",
        ));
    }
    None
}

fn collect_loops(list: &List, backgrounded: bool, out: &mut Vec<LoopInfo>) {
    for item in &list.items {
        let bg = backgrounded || item.background;
        for pipeline in item.pipelines() {
            for command in &pipeline.commands {
                collect_loops_command(command, bg, out);
            }
        }
    }
}

fn collect_loops_command(command: &Command, backgrounded: bool, out: &mut Vec<LoopInfo>) {
    match command {
        Command::For { words, body, .. } => {
            if !backgrounded {
                out.push(LoopInfo {
                    kind: LoopKind::For,
                    iters: for_iterations(words.as_deref()),
                    sleep_s: body_sleep(body),
                    always_true: false,
                });
            }
            collect_loops(body, backgrounded, out);
        }
        Command::Branches(BranchKind::While, lists, _) => {
            if !backgrounded {
                let sleep_s = lists.get(1).and_then(body_sleep);
                let always_true = lists.first().is_some_and(is_always_true);
                out.push(LoopInfo {
                    kind: LoopKind::While,
                    iters: None,
                    sleep_s,
                    always_true,
                });
            }
            if let Some(body) = lists.get(1) {
                collect_loops(body, backgrounded, out);
            }
        }
        Command::Branches(BranchKind::Until, lists, _) => {
            if let Some(body) = lists.get(1) {
                collect_loops(body, backgrounded, out);
            }
        }
        Command::Branches(BranchKind::If, lists, _) => {
            for list in lists {
                collect_loops(list, backgrounded, out);
            }
        }
        Command::Subshell(list, _) | Command::Group(list, _) => {
            collect_loops(list, backgrounded, out);
        }
        Command::Case { arms, .. } => {
            for (_, body) in arms {
                collect_loops(body, backgrounded, out);
            }
        }
        Command::Simple(_)
        | Command::Conditional(_)
        | Command::Arithmetic(_)
        | Command::Function { .. }
        | Command::Unparsed(_) => {}
    }
}

fn body_sleep(body: &List) -> Option<f64> {
    let mut best = None;
    walk_body_sleep(body, &mut |seconds| {
        best = Some(match best {
            Some(current) if current >= seconds => current,
            _ => seconds,
        });
    });
    best
}

fn walk_body_sleep(list: &List, visit: &mut dyn FnMut(f64)) {
    for item in &list.items {
        if item.background {
            continue;
        }
        for pipeline in item.pipelines() {
            for command in &pipeline.commands {
                walk_body_sleep_command(command, visit);
            }
        }
    }
}

fn walk_body_sleep_command(command: &Command, visit: &mut dyn FnMut(f64)) {
    match command {
        Command::Simple(simple) => {
            if let Some(seconds) = sleep_seconds(simple) {
                visit(seconds);
            }
        }
        Command::Subshell(list, _) | Command::Group(list, _) => walk_body_sleep(list, visit),
        Command::Branches(BranchKind::If, lists, _) => {
            for list in lists {
                walk_body_sleep(list, visit);
            }
        }
        Command::Case { arms, .. } => {
            for (_, body) in arms {
                walk_body_sleep(body, visit);
            }
        }
        // Nested loops and function definitions are inspected on their own.
        Command::Branches(BranchKind::While | BranchKind::Until, ..)
        | Command::For { .. }
        | Command::Function { .. }
        | Command::Conditional(_)
        | Command::Arithmetic(_)
        | Command::Unparsed(_) => {}
    }
}

fn sleep_seconds(command: &SimpleCommand) -> Option<f64> {
    if !command.assignments.is_empty() {
        return None;
    }
    let [program, amount] = command.words.as_slice() else {
        return None;
    };
    if program.raw != "sleep" {
        return None;
    }
    let amount = amount.as_static()?;
    parse_sleep_number(&amount)
}

fn parse_sleep_number(text: &str) -> Option<f64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    text.parse().ok()
}

fn for_iterations(words: Option<&[Word]>) -> Option<u64> {
    let words = words?;
    let mut total: u64 = 0;
    for word in words {
        total = total.checked_add(word_iterations(word)?)?;
    }
    Some(total)
}

fn word_iterations(word: &Word) -> Option<u64> {
    if word.parts.len() == 1 {
        match &word.parts[0] {
            Part::Literal(text, Quoting::Bare) => {
                if let Some(count) = brace_range_count(text) {
                    return Some(count);
                }
                if !text.contains(['*', '?', '[', '{']) {
                    return Some(1);
                }
                return None;
            }
            Part::Literal(_, _) => return Some(1),
            Part::Command { body, .. } => return seq_command_count(body),
            _ => return None,
        }
    }
    if word.has_expansion() {
        return None;
    }
    Some(1)
}

/// Count of `{LO..HI}` (step 1) when both bounds are non-negative integers;
/// `None` for any other brace expansion.
fn brace_range_count(text: &str) -> Option<u64> {
    let inner = text.strip_prefix('{')?.strip_suffix('}')?;
    let (lo, hi) = inner.split_once("..")?;
    let lo: u64 = lo.parse().ok()?;
    let hi: u64 = hi.parse().ok()?;
    Some(lo.abs_diff(hi) + 1)
}

/// Iteration count a `$(seq ...)` substitution produces when its arguments
/// are non-negative literal integers (`seq N`, `seq LO HI`); `None`
/// otherwise. A reverse or signed `seq` is left as unknown; the guard then
/// falls back to its per-iteration-sleep rule.
fn seq_command_count(body: &List) -> Option<u64> {
    let [item] = body.items.as_slice() else {
        return None;
    };
    if item.background || !item.rest.is_empty() {
        return None;
    }
    let [Command::Simple(command)] = item.first.commands.as_slice() else {
        return None;
    };
    if !command.assignments.is_empty() {
        return None;
    }
    let name = command.words.first()?.as_static()?;
    if name != "seq" {
        return None;
    }
    let args: Vec<u64> = command.words[1..]
        .iter()
        .map(|word| word.as_static().and_then(|text| text.parse().ok()))
        .collect::<Option<_>>()?;
    match args.as_slice() {
        [n] if *n >= 1 => Some(*n),
        [lo, hi] if hi >= lo => Some(hi - lo + 1),
        _ => None,
    }
}

fn top_level_watch(list: &List) -> Option<String> {
    for item in &list.items {
        if item.background {
            continue;
        }
        for pipeline in item.pipelines() {
            for command in &pipeline.commands {
                if let Some(shape) = find_top_level_watch(command) {
                    return Some(shape);
                }
            }
        }
    }
    None
}

fn find_top_level_watch(command: &Command) -> Option<String> {
    match command {
        Command::Simple(simple) => watch_command_shape(simple),
        Command::Subshell(list, _) | Command::Group(list, _) => top_level_watch(list),
        _ => None,
    }
}

const WATCH_DEFAULT_INTERVAL_SECONDS: f64 = 2.0;

fn watch_command_shape(command: &SimpleCommand) -> Option<String> {
    let program = command.words.first()?.as_static()?;
    let name = program.rsplit('/').next()?;
    if name != "watch" {
        return None;
    }
    let interval = watch_interval(&command.words[1..]);
    match interval {
        Some(seconds) => Some(format!("`watch` repeating every {seconds} seconds")),
        None => Some(format!(
            "`watch` repeating every {WATCH_DEFAULT_INTERVAL_SECONDS} seconds (its default)"
        )),
    }
}

/// `-n S` / `-nS` / `--interval S` / `--interval=S` when `S` is a literal
/// number; `None` if the flag is missing, its argument is dynamic, or the
/// argument list breaks with `--`.
fn watch_interval(args: &[Word]) -> Option<f64> {
    let mut iter = args.iter();
    while let Some(word) = iter.next() {
        let Some(text) = word.as_static() else {
            continue;
        };
        if text == "--" {
            break;
        }
        if text == "-n" || text == "--interval" {
            let next = iter.next().and_then(Word::as_static)?;
            return parse_sleep_number(&next);
        }
        if let Some(value) = text.strip_prefix("--interval=") {
            return parse_sleep_number(value);
        }
        if let Some(value) = text.strip_prefix("-n").filter(|rest| !rest.is_empty()) {
            return parse_sleep_number(value);
        }
    }
    None
}

fn is_always_true(condition: &List) -> bool {
    let [item] = condition.items.as_slice() else {
        return false;
    };
    if item.background || !item.rest.is_empty() || item.first.negated {
        return false;
    }
    let [command] = item.first.commands.as_slice() else {
        return false;
    };
    match command {
        Command::Simple(simple) => {
            if !simple.assignments.is_empty() {
                return false;
            }
            let [word] = simple.words.as_slice() else {
                return false;
            };
            matches!(word.as_static().as_deref(), Some(":" | "true"))
        }
        Command::Arithmetic(word) => word
            .as_static()
            .is_some_and(|text| text.trim().parse::<i64>().is_ok_and(|n| n != 0)),
        _ => false,
    }
}
