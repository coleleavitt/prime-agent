//! Full-environment dumps (`env`, `printenv`, `export -p`), the `env`
//! command lines that run another command, and the one filtered form that
//! stays allowed: a dump that really feeds `grep` for one fixed string.

use super::mask::{Segment, WORD_BREAKERS};
use super::words::{
    all_digits, analysis_words, brace_descriptor_len, is_assignment_word, is_digit, split_blanks,
};

/// Commands that print a whole environment when given no other word.
const DUMP_COMMANDS: [&str; 2] = ["env", "printenv"];

/// How deep the `env -S` operand re-test follows nested command lines
/// before answering as a dump (fail closed, and no unbounded recursion).
const DUMP_NESTING_LIMIT: usize = 16;

/// The longest digit run a shell descriptor can name.
const MAX_DESCRIPTOR_DIGITS: usize = 9;

/// `env` flags whose next word is their operand, not a command.
const ENV_OPERAND_FLAGS: [&str; 4] = ["-u", "--unset", "-C", "--chdir"];
/// `env` flags whose operand is a command line of its own.
const ENV_SPLIT_FLAGS: [&str; 2] = ["-S", "--split-string"];

/// The only characters that may sit between a descriptor and its operator.
const SHELL_METACHARACTERS: &str = "|&;()<> \t\n";

/// A grep pattern with any of these is a regex, not provably bounded.
const GREP_PATTERN_METACHARS: &str = ".*[](){}^$\\|?+`";
/// Short grep flags that unbound the output: invert, pattern file, context
/// lines, NUL-delimited records.
const GREP_UNBOUNDED_FLAG_LETTERS: &str = "vfABCz";
/// Long spellings of the same; grep takes any unambiguous prefix.
const GREP_UNBOUNDED_LONG_FLAGS: [&str; 6] = [
    "--invert-match",
    "--file",
    "--after-context",
    "--before-context",
    "--context",
    "--null-data",
];
const GREP_MAX_COUNT_LONG_FLAG: &str = "--max-count";
const GREP_PATTERN_LONG_FLAG: &str = "--regexp";

/// `(name, separator present, value)` of `word.partition("=")`.
fn partition_eq(word: &str) -> (&str, bool, &str) {
    match word.split_once('=') {
        Some((name, value)) => (name, true, value),
        None => (word, false, ""),
    }
}

/// The words of an `env` line minus the operand each `-u`/`-C` flag owns
/// (glued `--unset=PATH` carries it in the word).
fn env_flag_operands_dropped(words: &[String]) -> Vec<String> {
    let mut remaining = Vec::new();
    let mut expect_operand = false;
    for word in words {
        if expect_operand {
            expect_operand = false;
            continue;
        }
        if ENV_OPERAND_FLAGS.contains(&word.as_str()) {
            expect_operand = true;
            continue;
        }
        if ENV_OPERAND_FLAGS.contains(&partition_eq(word).0) {
            continue;
        }
        remaining.push(word.clone());
    }
    remaining
}

/// The words of an `-S`/`--split-string` operand (`None` when there is
/// none). An operand that splits to nothing is an empty list: the bare `env`
/// the caller must refuse.
pub(super) fn env_split_words(words: &[String]) -> Option<Vec<String>> {
    words.iter().enumerate().find_map(|(index, word)| {
        let (name, separator, value) = partition_eq(word);
        if !ENV_SPLIT_FLAGS.contains(&name) {
            return None;
        }
        let value = if separator {
            value
        } else {
            words.get(index + 1).map_or("", String::as_str)
        };
        Some(split_blanks(value))
    })
}

/// The command line the words of an `env` invocation run: `-u`/`-C`
/// operands, leading assignments and flags dropped, or the `-S` operand as
/// the whole command line.
pub(super) fn executed_command_words(words: &[String]) -> Vec<String> {
    let mut rest = env_flag_operands_dropped(words);
    let assignments = rest
        .iter()
        .take_while(|word| is_assignment_word(word))
        .count();
    rest.drain(..assignments);
    match env_split_words(&rest) {
        Some(split) if !split.is_empty() => split,
        Some(_) | None => rest
            .into_iter()
            .filter(|word| !word.starts_with('-'))
            .collect(),
    }
}

/// Whether these (non-empty) words print a whole environment with no filter.
pub(super) fn is_bare_dump(words: &[String]) -> bool {
    bare_dump_at(words, 0)
}

fn bare_dump_at(words: &[String], depth: usize) -> bool {
    if depth > DUMP_NESTING_LIMIT {
        return true;
    }
    let command = words[0].as_str();
    if DUMP_COMMANDS.contains(&command) {
        let mut rest = words[1..].to_vec();
        if command == "env" {
            rest = env_flag_operands_dropped(&rest)
                .into_iter()
                .filter(|word| !is_assignment_word(word))
                .collect();
            // `env printenv` and `env -S 'env'` run another dump word; the
            // nested command line is read the same way.
            let nested = rest.iter().enumerate().any(|(position, word)| {
                DUMP_COMMANDS.contains(&word.as_str()) && bare_dump_at(&rest[position..], depth + 1)
            });
            if nested {
                return true;
            }
            if let Some(split) = env_split_words(&rest) {
                if split.is_empty() || bare_dump_at(&split, depth + 1) {
                    return true;
                }
            }
        }
        // Nothing but flags after the command word.
        return rest.iter().all(|word| word.starts_with('-'));
    }
    if command == "export" {
        // Only the flag-only forms dump; a cluster of `f`s prints definitions.
        if words[1..].iter().any(|word| !word.starts_with('-')) {
            return false;
        }
        let mut flags = words[1..].iter().peekable();
        return flags.peek().is_none()
            || flags.any(|flag| flag.len() == 1 || flag[1..].chars().any(|ch| ch != 'f'));
    }
    false
}

/// Whether an `-m`/`--max-count` value caps the output: a digit run with a
/// non-zero digit (`-m0` prints the whole dump here).
fn max_count_bounds_output(value: &str) -> bool {
    all_digits(value) && value.chars().any(|ch| ch != '0')
}

/// How one short-flag cluster affects the bounded-filter test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClusterEffect {
    /// Unbounded or widening.
    Wide,
    /// Ends with an `m` whose count is the next word (`-m 1`).
    Value,
    /// A bounded flag.
    Bounded,
}

fn cluster_flag_effect(cluster: &[char]) -> ClusterEffect {
    let mut index = 0;
    while index < cluster.len() {
        let ch = cluster[index];
        if ch == 'e' && index + 1 < cluster.len() {
            // A glued pattern value: the pattern is unread, the rest a file.
            return ClusterEffect::Wide;
        }
        if ch == 'm' {
            let mut digits = index + 1;
            while digits < cluster.len() && is_digit(cluster[digits]) {
                digits += 1;
            }
            if digits == cluster.len() && digits == index + 1 {
                return ClusterEffect::Value;
            }
            let count: String = cluster[index + 1..digits].iter().collect();
            if !max_count_bounds_output(&count) {
                return ClusterEffect::Wide;
            }
            index = digits;
            continue;
        }
        if GREP_UNBOUNDED_FLAG_LETTERS.contains(ch) || is_digit(ch) {
            return ClusterEffect::Wide;
        }
        index += 1;
    }
    ClusterEffect::Bounded
}

/// Whether these words grep for exactly one fixed string: a single plain
/// operand, no widening flag (short, clustered, `-NUM`, or a long prefix),
/// no glued pattern, and a `--` ending the options the way grep reads it.
pub(super) fn is_bounded_grep_filter(words: &[String]) -> bool {
    if words.first().map(String::as_str) != Some("grep") {
        return false;
    }
    let flags = &words[1..];
    let mut operands: Vec<&str> = Vec::new();
    let mut index = 0;
    let mut options_ended = false;
    while index < flags.len() {
        let word = flags[index].as_str();
        index += 1;
        if options_ended || !word.starts_with('-') {
            operands.push(word);
            continue;
        }
        if word.starts_with("--") {
            if word == "--" {
                options_ended = true;
                continue;
            }
            let (name, separator, glued) = partition_eq(word);
            if GREP_UNBOUNDED_LONG_FLAGS
                .iter()
                .any(|flag| flag.starts_with(name))
            {
                return false;
            }
            if separator && GREP_PATTERN_LONG_FLAG.starts_with(name) {
                return false;
            }
            if name == GREP_MAX_COUNT_LONG_FLAG
                || (separator && GREP_MAX_COUNT_LONG_FLAG.starts_with(name))
            {
                let value = if separator {
                    glued
                } else {
                    let value = flags.get(index).map_or("", String::as_str);
                    if !all_digits(value) {
                        continue;
                    }
                    index += 1;
                    value
                };
                if !all_digits(value) {
                    continue;
                }
                if !max_count_bounds_output(value) {
                    return false;
                }
            }
            continue;
        }
        let cluster: Vec<char> = word.chars().skip(1).collect();
        match cluster_flag_effect(&cluster) {
            ClusterEffect::Wide => return false,
            ClusterEffect::Value => {
                // A non-numeric spaced count makes grep fail: nothing prints.
                let value = flags.get(index).map_or("", String::as_str);
                if !all_digits(value) {
                    continue;
                }
                index += 1;
                if !max_count_bounds_output(value) {
                    return false;
                }
            }
            ClusterEffect::Bounded => {}
        }
    }
    match operands.as_slice() {
        [pattern] if !pattern.is_empty() => !pattern
            .chars()
            .any(|ch| GREP_PATTERN_METACHARS.contains(ch)),
        _ => false,
    }
}

/// The words of the segment a pipe feeds. A wordless segment a newline ends
/// continues the pipe (`env |` then `grep KEY` on the next line); any other
/// wordless segment (`env || grep KEY`) is no filter at all.
pub(super) fn pipe_follower_words(
    command: &[char],
    segments: &[Segment],
    from: usize,
) -> Vec<String> {
    for segment in &segments[from..] {
        let words = analysis_words(command, segment.start, segment.end);
        if !words.is_empty() {
            return words;
        }
        if segment.separator != Some('\n') {
            return Vec::new();
        }
    }
    Vec::new()
}

/// How a descriptor written before an operator affects fd 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DescriptorEffect {
    /// Absent or fd 1: the redirection acts on stdout.
    Written,
    /// Another descriptor: fd 1 keeps the pipe.
    Other,
    /// No descriptor this scan can read (fail closed: moves fd 1).
    Unreadable,
}

fn descriptor_effect(digits: &str) -> DescriptorEffect {
    if digits.is_empty() {
        return DescriptorEffect::Written;
    }
    let stripped = digits.trim_start_matches('0');
    if stripped.chars().count() > MAX_DESCRIPTOR_DIGITS {
        DescriptorEffect::Unreadable
    } else if stripped == "1" {
        DescriptorEffect::Written
    } else {
        DescriptorEffect::Other
    }
}

/// The descriptor written right before the operator at `operator`: a digit
/// run (or a `{name}`) that starts its own token; the `2` of `a2>&2` names
/// none.
fn descriptor_text(masked: &[char], operator: usize) -> String {
    let mut source = operator;
    while source > 0 && is_digit(masked[source - 1]) {
        source -= 1;
    }
    if source == operator && source > 0 && masked[source - 1] == '}' {
        if let Some(brace) = masked[..source].iter().rposition(|ch| *ch == '{') {
            if brace_descriptor_len(&masked[brace..source]) == Some(source - brace)
                && (brace == 0 || SHELL_METACHARACTERS.contains(masked[brace - 1]))
            {
                return masked[brace..source].iter().collect();
            }
        }
        return String::new();
    }
    if source > 0 && !SHELL_METACHARACTERS.contains(masked[source - 1]) {
        return String::new();
    }
    masked[source..operator].iter().collect()
}

/// Whether a redirection in this masked segment takes fd 1 off the pipe
/// (`env >&2 | grep KEY` writes the dump to stderr, `env >log | ...` to a
/// file). Only a plain unquoted digit run is trusted as a duplicated
/// descriptor; anything else counts as moving fd 1.
pub(super) fn leaves_the_pipe(masked: &[char]) -> bool {
    let mut index = 0;
    while index < masked.len() {
        let Some(found) = masked[index..].iter().position(|ch| *ch == '>') else {
            return false;
        };
        index += found;
        let before = index.checked_sub(1).map(|at| masked[at]);
        if before == Some('&') && (index < 2 || SHELL_METACHARACTERS.contains(masked[index - 2])) {
            // `&>` and `&>>` send both streams away.
            return true;
        }
        if before == Some('<') {
            // `<>` replaces the descriptor it names; fd 1 or none leaves.
            if descriptor_effect(&descriptor_text(masked, index - 1)) != DescriptorEffect::Other {
                return true;
            }
            index += 1;
            continue;
        }
        let duplicated = masked.get(index + 1) == Some(&'&');
        match descriptor_effect(&descriptor_text(masked, index)) {
            DescriptorEffect::Unreadable => return true,
            DescriptorEffect::Other => {
                index += if duplicated { 2 } else { 1 };
                continue;
            }
            DescriptorEffect::Written => {}
        }
        if !duplicated {
            return true;
        }
        let mut target = index + 2;
        while target < masked.len() && matches!(masked[target], ' ' | '\t') {
            target += 1;
        }
        let digits = target;
        while target < masked.len() && is_digit(masked[target]) {
            target += 1;
        }
        let descriptor: String = masked[digits..target].iter().collect();
        let ending = masked.get(target);
        if descriptor.is_empty() || ending.is_some_and(|ch| !WORD_BREAKERS.contains(*ch)) {
            return true;
        }
        if descriptor_effect(&descriptor) != DescriptorEffect::Written {
            return true;
        }
        index = target;
    }
    false
}
