//! Finding recursive chmod/chown invocations in scanned words, the `hash -p`
//! registrations that make other names run them, and the operand words of
//! one invocation.

use std::collections::BTreeSet;

use super::normalize::fold_ansi_c_span;
use super::patterns::{
    has_expandable_glob, has_glob_or_substitution, has_unresolved_expansion, is_assignment_word,
};
use super::pyos::{basename, is_space};
use super::vocabulary::{is_chmod_chown_word, is_recursive_token_run, COMMAND_SLOT_NOISE};
use super::words::{run_followers, run_tokens_from, ShellWord};

/// One recursive chmod/chown invocation: the span from its command word to
/// its last word, and the command word's index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Invocation {
    pub start: usize,
    pub end: usize,
    pub word_index: usize,
}

/// Every recursive chmod/chown invocation (or one whose flag region carries
/// an expansion that could become the recursive flag). `hash_aliases` are
/// names a `hash -p` registration points at chmod/chown.
pub(super) fn find_invocations(
    words: &[ShellWord],
    hash_aliases: Option<&BTreeSet<String>>,
) -> Vec<Invocation> {
    let mut invocations = Vec::new();
    for (index, word) in words.iter().enumerate() {
        if !is_chmod_chown_word(&word.value)
            && !hash_aliases.is_some_and(|names| names.contains(&word.value))
        {
            continue;
        }
        let mut end = word.end;
        let mut tokens = vec![word.value.as_str()];
        for follower in run_followers(words, index) {
            tokens.push(&follower.value);
            end = follower.end;
        }
        let invocation = Invocation {
            start: word.start,
            end,
            word_index: index,
        };
        if is_recursive_token_run(&tokens) {
            invocations.push(invocation);
            continue;
        }
        for token in &tokens[1..] {
            let expands = has_glob_or_substitution(token);
            if !token.starts_with('-') && !expands {
                break;
            }
            if expands {
                invocations.push(invocation);
                break;
            }
        }
    }
    invocations
}

/// Whether the `hash` word at `index` holds its command slot: assignments,
/// grouping tokens and option words before it pass the slot through.
fn holds_command_slot(words: &[ShellWord], index: usize) -> bool {
    if words[index].starts_command {
        return true;
    }
    for before in words[..index].iter().rev() {
        if is_assignment_word(&before.value) || COMMAND_SLOT_NOISE.contains(&before.value.as_str())
        {
            if before.starts_command {
                return true;
            }
            continue;
        }
        if before.value.starts_with('-') && before.value != "-" {
            continue;
        }
        return false;
    }
    true
}

/// The `hash -p` registrations in the words: the names pointed at
/// chmod/chown, and whether any registration is unreadable (its pathname or
/// name is built from expansion, or its pathname is a glob).
pub(super) fn hash_registered_command_names(words: &[ShellWord]) -> (BTreeSet<String>, bool) {
    let mut aliased = BTreeSet::new();
    let mut unreadable = false;
    for (index, word) in words.iter().enumerate() {
        if basename(&word.value) != "hash" || !holds_command_slot(words, index) {
            continue;
        }
        let mut has_pathname_option = false;
        let mut attached: Option<String> = None;
        let mut operands: Vec<String> = Vec::new();
        for token in run_tokens_from(words, index).into_iter().skip(1) {
            if token.starts_with('-') && token != "-" && !token.starts_with("--") {
                if let Some(position) = token[1..].find('p') {
                    has_pathname_option = true;
                    let value = &token[position + 2..];
                    if !value.is_empty() {
                        attached = Some(value.to_string());
                    }
                }
                continue;
            }
            if token.starts_with("--") {
                continue;
            }
            if !has_pathname_option {
                break;
            }
            operands.push(token);
            if operands.len() == 2 {
                break;
            }
        }
        if !has_pathname_option {
            continue;
        }
        let (pathname, name) = match (attached, operands.as_slice()) {
            (Some(attached), [name, ..]) => (attached, name.clone()),
            (None, [pathname, name]) => (pathname.clone(), name.clone()),
            (Some(_) | None, _) => continue,
        };
        if has_unresolved_expansion(&format!("{pathname}{name}")) || has_expandable_glob(&pathname)
        {
            unreadable = true;
        } else if is_chmod_chown_word(&pathname) {
            aliased.insert(name);
        }
    }
    (aliased, unreadable)
}

/// Split one invocation region into words the way the shell passes them:
/// `None` for a word the resolver must not guess at (a substitution inside
/// double quotes, a process substitution or stray operator). The flag is
/// false when the region ended mid-quote.
pub(super) fn region_words(region: &[char]) -> (Vec<Option<String>>, bool) {
    let mut words: Vec<Option<String>> = Vec::new();
    let mut current = String::new();
    let mut unknown = false;
    let mut well_formed = true;
    let n = region.len();
    let flush = |words: &mut Vec<Option<String>>, current: &mut String, unknown: &mut bool| {
        if !current.is_empty() {
            words.push((!*unknown).then(|| current.clone()));
        }
        current.clear();
        *unknown = false;
    };
    // The body of a double-quoted span starting at `from` (just past the
    // quote): escapes before `"$`\` fold, the index of the closing quote.
    let double_quoted = |from: usize| -> Option<(String, usize)> {
        let mut end = from;
        while end < n {
            let c = region[end];
            if c == '\\' && end + 1 < n && "\"$`\\".contains(region[end + 1]) {
                end += 2;
                continue;
            }
            if c == '"' {
                let mut body = String::new();
                let mut k = from;
                while k < end {
                    if region[k] == '\\' && k + 1 < end && "\"$`\\".contains(region[k + 1]) {
                        body.push(region[k + 1]);
                        k += 2;
                    } else {
                        body.push(region[k]);
                        k += 1;
                    }
                }
                return Some((body, end));
            }
            end += 1;
        }
        None
    };
    let mut i = 0;
    while i < n && well_formed {
        let ch = region[i];
        if is_space(ch) {
            flush(&mut words, &mut current, &mut unknown);
            i += 1;
        } else if ch == '\\' && i + 1 < n {
            current.push(region[i + 1]);
            i += 2;
        } else if ch == '$' && region.get(i + 1) == Some(&'\'') {
            let (folded, next) = fold_ansi_c_span(region, i, n);
            current.push_str(&folded);
            i = next;
        } else if ch == '"' || (ch == '$' && region.get(i + 1) == Some(&'"')) {
            let from = if ch == '"' { i + 1 } else { i + 2 };
            let Some((body, close)) = double_quoted(from) else {
                well_formed = false;
                break;
            };
            if has_unresolved_expansion(&body) {
                unknown = true;
            }
            current.push_str(&body);
            i = close + 1;
        } else if ch == '\'' {
            let Some(offset) = region[i + 1..].iter().position(|c| *c == '\'') else {
                well_formed = false;
                break;
            };
            let end = i + 1 + offset;
            current.extend(&region[i + 1..end]);
            i = end + 1;
        } else if ch == '#' && current.is_empty() {
            break;
        } else if ";&|\n)".contains(ch) {
            flush(&mut words, &mut current, &mut unknown);
            break;
        } else if "(<>".contains(ch) {
            flush(&mut words, &mut current, &mut unknown);
            words.push(None);
            break;
        } else {
            current.push(ch);
            i += 1;
        }
    }
    flush(&mut words, &mut current, &mut unknown);
    (words, well_formed)
}

/// The operand words of one invocation: options skipped (with the values of
/// `--reference`/`--from`), every word after `--` an operand, the mode (or
/// owner) first; an unterminated region adds one unresolvable operand.
pub(super) fn operand_words(words: &[Option<String>], well_formed: bool) -> Vec<Option<String>> {
    let mut operands = Vec::new();
    let mut after_ddash = false;
    let mut skip_value = false;
    for word in words.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        if !after_ddash && word.as_deref() == Some("--") {
            after_ddash = true;
            continue;
        }
        if let Some(text) = word.as_deref() {
            if !after_ddash && text.starts_with('-') && text != "-" {
                if text == "--reference" || text == "--from" {
                    skip_value = true;
                }
                continue;
            }
        }
        operands.push(word.clone());
    }
    if !well_formed {
        operands.push(None);
    }
    operands
}

/// The command-executing wrappers of one run, in execution order, with the
/// tokens each consumes before handing over to the next.
pub(super) fn wrapper_chain_groups(run_words: &[String]) -> Vec<(String, Vec<String>)> {
    use super::vocabulary::UNRESOLVABLE_COMMAND_EXECUTORS as EXECUTORS;
    let mut groups = Vec::new();
    let mut index = 0;
    while index < run_words.len() {
        let word = &run_words[index];
        if COMMAND_SLOT_NOISE.contains(&word.as_str()) || is_assignment_word(word) {
            index += 1;
            continue;
        }
        if word.starts_with('-') && word != "-" {
            index += 1;
            continue;
        }
        let name = basename(word);
        if !EXECUTORS.contains(&name) {
            break;
        }
        let name = name.to_string();
        let mut tokens = Vec::new();
        index += 1;
        while index < run_words.len() && !EXECUTORS.contains(&basename(&run_words[index])) {
            tokens.push(run_words[index].clone());
            index += 1;
        }
        groups.push((name, tokens));
    }
    groups
}

/// Every value `env` passes for one of its value options (`-C dir`, `-Cdir`,
/// `-iC/`, `-iC dir`, `--chdir=dir`, any unambiguous long prefix).
pub(super) fn env_option_values(tokens: &[String], short: char, long: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if let Some(rest) = token.strip_prefix("--") {
            let (name, inline) = rest.split_once('=').unwrap_or((rest, ""));
            let matches_long = !name.is_empty() && long.starts_with(name);
            if matches_long && inline.is_empty() && index + 1 < tokens.len() {
                values.push(tokens[index + 1].clone());
                index += 1;
            } else if matches_long && !inline.is_empty() {
                values.push(inline.to_string());
            }
            index += 1;
            continue;
        }
        if token.starts_with('-') && token != "-" {
            let cluster = &token[1..];
            if let Some(position) = cluster.find(short) {
                let attached = &cluster[position + short.len_utf8()..];
                if !attached.is_empty() {
                    values.push(attached.to_string());
                } else if index + 1 < tokens.len() {
                    values.push(tokens[index + 1].clone());
                    index += 1;
                }
            }
        }
        index += 1;
    }
    values
}

/// The argv text `env -S` splits its string into, or `None` when the string
/// carries expansion: whitespace splits outside quotes, backslashes escape,
/// `\_` is a separator.
pub(super) fn split_env_string(value: &str) -> Option<String> {
    if has_unresolved_expansion(value) {
        return None;
    }
    let chars: Vec<char> = value.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            if chars[i + 1] == '_' {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            } else {
                current.push(chars[i + 1]);
            }
            i += 2;
            continue;
        }
        if quote.is_none() && (ch == '\'' || ch == '"') {
            quote = Some(ch);
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && is_space(ch) {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
        i += 1;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Some(parts.join(" "))
}

/// The split argv texts of every `env -S/--split-string` operand, and
/// whether any operand carries expansion (then it is reported, not scanned).
pub(super) fn env_split_string_feeds(words: &[ShellWord]) -> (Vec<String>, bool) {
    let mut feeds = Vec::new();
    let mut expansion = false;
    for (index, word) in words.iter().enumerate() {
        if basename(&word.value) != "env" {
            continue;
        }
        let tokens = run_tokens_from(words, index);
        for value in env_option_values(&tokens[1..], 'S', "split-string") {
            if super::pyos::strip(&value).is_empty() {
                continue;
            }
            match split_env_string(&value) {
                Some(split) => feeds.push(split),
                None => expansion = true,
            }
        }
    }
    (feeds, expansion)
}
