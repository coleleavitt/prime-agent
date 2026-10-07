//! Configuration a command writes for itself: `remote.<name>.mirror` and
//! `remote.<name>.push` turn a plain push into a forced one, in any of the
//! spellings git reads (an inline `-c`, a `git config` argument, the
//! `GIT_CONFIG_KEY_<i>` / `GIT_CONFIG_PARAMETERS` environment), and an inline
//! config operand the guard cannot read hides which configuration applies.

use std::collections::HashMap;

use super::messages;
use super::push::{PushRun, GIT_GLOBAL_VALUE_LONG, GIT_GLOBAL_VALUE_SHORT};
use super::text::{has_glob_or_substitution, is_assignment_name};
use super::words::ShellWord;

/// Builtins whose operands are assignments.
const EXPORT_BUILTINS: [&str; 5] = ["export", "declare", "typeset", "local", "readonly"];

/// The value a variable is assigned, `None` when it carries an expansion or
/// a glob (not the literal text the shell passes).
pub(super) type Assignments = HashMap<String, Option<String>>;

/// `$`-anchor semantics: the end of the text, or a final newline.
fn at_line_end(rest: &str) -> bool {
    rest.is_empty() || rest == "\n"
}

/// Strip an ASCII case-insensitive `prefix`.
fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let head = value.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &value[prefix.len()..])
}

/// `^remote\.[^.]+\.(?:mirror|push)(?:=|$)`, case-insensitively (git reads
/// the section and variable names case-insensitively).
pub(super) fn is_mirror_or_push_key(value: &str) -> bool {
    let Some(rest) = strip_prefix_ignore_case(value, "remote.") else {
        return false;
    };
    let Some(dot) = rest.find('.') else {
        return false;
    };
    if dot == 0 {
        return false;
    }
    let variable = &rest[dot + 1..];
    ["mirror", "push"].iter().any(|name| {
        strip_prefix_ignore_case(variable, name)
            .is_some_and(|after| after.starts_with('=') || at_line_end(after))
    })
}

/// The key of a `GIT_CONFIG_KEY_<n>=<key>` word (`.*$`: up to a newline that
/// may only end the word).
fn env_config_key(value: &str) -> Option<&str> {
    let rest = strip_prefix_ignore_case(value, "GIT_CONFIG_KEY_")?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let key = rest[digits..].strip_prefix('=')?;
    match key.find('\n') {
        None => Some(key),
        Some(newline) if newline + 1 == key.len() => Some(&key[..newline]),
        Some(_) => None,
    }
}

/// True when a word sets `remote.<name>.mirror` or `remote.<name>.push`.
pub(super) fn mirror_or_push_refspec_configured(words: &[ShellWord]) -> bool {
    words.iter().any(|word| {
        let value = word.value.as_str();
        is_mirror_or_push_key(value)
            || strip_prefix_ignore_case(value, "GIT_CONFIG_PARAMETERS=").is_some()
            || env_config_key(value)
                .is_some_and(|key| is_mirror_or_push_key(key) || has_glob_or_substitution(key))
    })
}

/// Whether `words[index]` is in assignment position: the first word of a
/// command, or the operand of an export-style builtin.
fn in_assignment_position(words: &[ShellWord], index: usize) -> bool {
    let previous = if index > 0 {
        words[index - 1].value.as_str()
    } else {
        ""
    };
    words[index].starts_command || EXPORT_BUILTINS.contains(&previous)
}

/// NAME -> value for the assignments made before `words[before]`.
pub(super) fn literal_assignments(words: &[ShellWord], before: usize) -> Assignments {
    let mut assignments = Assignments::new();
    for index in 0..before.min(words.len()) {
        let Some((name, assigned)) = words[index].value.split_once('=') else {
            continue;
        };
        if !is_assignment_name(name) || !in_assignment_position(words, index) {
            continue;
        }
        assignments.insert(
            name.to_string(),
            (!has_glob_or_substitution(assigned)).then(|| assigned.to_string()),
        );
    }
    assignments
}

/// The HOME and CDPATH the command's own `cd` commands read, trusted only
/// when each is assigned at most once, before the first cd, without a `~`
/// the shell would expand; otherwise both are unreadable (`None`). A name the
/// command never assigns is absent.
pub(super) fn cd_environment(words: &[ShellWord], limit: usize) -> Assignments {
    let first_cd = (0..limit)
        .find(|index| matches!(words[*index].value.as_str(), "cd" | "pushd"))
        .unwrap_or(limit);
    let mut tracked = Assignments::new();
    for index in 0..limit.min(words.len()) {
        let Some((name, assigned)) = words[index].value.split_once('=') else {
            continue;
        };
        if !matches!(name, "HOME" | "CDPATH") || !in_assignment_position(words, index) {
            continue;
        }
        if index >= first_cd || tracked.contains_key(name) || assigned.starts_with('~') {
            tracked.insert("HOME".to_string(), None);
            tracked.insert("CDPATH".to_string(), None);
            break;
        }
        tracked.insert(
            name.to_string(),
            (!has_glob_or_substitution(assigned)).then(|| assigned.to_string()),
        );
    }
    tracked
}

/// `^\$\{?NAME\}?$`: the variable a word is exactly made of.
fn simple_variable(word: &str) -> Option<&str> {
    let rest = word.strip_prefix('$')?;
    let rest = rest.strip_prefix('{').unwrap_or(rest);
    let name = rest.strip_suffix('}').unwrap_or(rest);
    is_assignment_name(name).then_some(name)
}

/// The literal value this command assigns to the variable `word` is.
fn config_variable_value<'a>(word: &str, assignments: &'a Assignments) -> Option<&'a str> {
    assignments.get(simple_variable(word)?)?.as_deref()
}

/// The config key a `-c`/`--config-env` operand writes, or `None` when the
/// guard cannot read it. Only the key decides which configuration is
/// written, so a dynamic value under a literal key stays readable.
fn inline_config_key(word: &str, assignments: &Assignments) -> Option<String> {
    if let Some((key, _)) = word.split_once('=') {
        if !key.is_empty() {
            if !has_glob_or_substitution(key) {
                return Some(key.to_string());
            }
            let resolved = config_variable_value(key, assignments)?;
            return inline_config_key(&format!("{resolved}="), assignments);
        }
    }
    let resolved = config_variable_value(word, assignments)?;
    inline_config_key(resolved, assignments)
}

/// Why this invocation carries inline config the guard cannot read (or that
/// arms a force push), as the refusal message; `None` when it carries none.
pub(super) fn unreadable_inline_config(run: &PushRun, words: &[ShellWord]) -> Option<String> {
    let assignments = literal_assignments(words, run.git_index);
    let tokens = &run.tokens;
    let mut index = 1;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if token == "--" || !token.starts_with('-') || token == "-" {
            return None;
        }
        let mut operand: Option<&str> = None;
        let mut from_environment = false;
        if token == "-c" || token == "--config-env" {
            operand = tokens.get(index + 1).map(String::as_str);
            from_environment = token == "--config-env";
            index += 2;
        } else if let Some(rest) = token.strip_prefix("--config-env=") {
            operand = Some(rest);
            from_environment = true;
            index += 1;
        } else if token.starts_with("-c") && token.len() > 2 {
            operand = Some(&token[2..]);
            index += 1;
        } else if GIT_GLOBAL_VALUE_SHORT.contains(&token) || GIT_GLOBAL_VALUE_LONG.contains(&token)
        {
            index += 2;
        } else {
            index += 1;
        }
        let Some(operand) = operand else {
            continue;
        };
        let Some(key) = inline_config_key(operand, &assignments) else {
            return Some(messages::config_option_refusal(operand));
        };
        if is_mirror_or_push_key(&key) {
            return Some(messages::mirror_config_refusal());
        }
        if from_environment {
            let variable = operand.split_once('=').map_or("", |(_, variable)| variable);
            if !is_assignment_name(variable)
                || assignments.get(variable).is_none_or(Option::is_none)
            {
                return Some(messages::config_option_refusal(operand));
            }
        }
    }
    None
}
