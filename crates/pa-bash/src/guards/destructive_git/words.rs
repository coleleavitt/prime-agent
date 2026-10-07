//! Shell words and the position the shell gives each one, and the rebuilt
//! text in which every command word reads the way the shell executes it
//! (`"git"`, `g'it'`, `$G` after `G=git`, an alias the text defined).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use super::text::{equals, starts_comment, string, strip_shell_escapes};
use crate::syntax::chars::is_space;
use crate::syntax::pyre::{Haystack, PyRegex};

/// Names to values (assignments or aliases) the walk knows about.
pub(super) type Names = BTreeMap<String, String>;

/// Commands whose arguments the shell applies as assignments that persist.
const EXPORT_COMMANDS: [&str; 5] = ["export", "declare", "typeset", "local", "readonly"];
/// Builtins that run the next word as a command themselves.
pub(super) const TRANSPARENT_BUILTINS: [&str; 2] = ["command", "builtin"];
/// Reserved words that introduce a command instead of being one.
pub(super) const SHELL_KEYWORDS: [&str; 19] = [
    "{", "}", "!", "if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in",
    "case", "esac", "select", "time", "function",
];

pub(super) static PLAIN_WORD_RUN: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"[A-Za-z0-9_./-]+"));
static VARIABLE_REFERENCE: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)\b|\{([A-Za-z_][A-Za-z0-9_]*)\})")
});
pub(super) static LITERAL_ASSIGNMENT: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(r#"([A-Za-z_][A-Za-z0-9_]*)=(?:"([^"$`]*)"|'([^']*)'|([A-Za-z0-9_./-]+))"#)
});
static COPIED_ASSIGNMENT: LazyLock<PyRegex> = LazyLock::new(|| {
    PyRegex::new(
        r#"([A-Za-z_][A-Za-z0-9_]*)=(?:"?\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z_][A-Za-z0-9_]*)\})"?)"#,
    )
});
pub(super) static REPLAYABLE_ASSIGNMENT: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r#"[A-Za-z_][A-Za-z0-9_]*=[^\s$`;&|()<>"]+"#));
pub(super) static ASSIGNMENT_WORD: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r#"[A-Za-z_][A-Za-z0-9_]*=(?:"[^"]*"|'[^']*'|[^\s;&|()<>"']*)"#));

fn is_one_of(word: &[char], set: &[&str]) -> bool {
    set.iter().any(|item| equals(word, item))
}

/// One shell word and the position the shell gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about one word's shell position"
)]
pub(super) struct ShellWord {
    pub start: usize,
    pub end: usize,
    /// The shell reads a `NAME=value` word here.
    pub assignment: bool,
    /// The name stays set after the command (`export G=git` and siblings).
    pub keeps: bool,
    /// The word the shell would execute.
    pub command: bool,
    /// The simple command still has no command word after this word.
    pub open_prefix: bool,
}

/// Spans of the shell words with the position each holds. Quotes never end a
/// word, braces stay inside one, comments are skipped, the `export` family
/// keeps its own options in the assignment run, and `command`/`builtin` run
/// the next word without taking the command word.
pub(super) fn shell_word_positions(command: &[char]) -> Vec<ShellWord> {
    let mut words = Vec::new();
    let mut assignment_slot = true;
    let mut command_word = true;
    let mut export_args = false;
    let mut prefix_open = true;
    let mut function_name = false;
    let n = command.len();
    let mut i = 0;
    while i < n {
        let ch = command[i];
        if ch == '#' && starts_comment(command, i) {
            i = command[i..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(n, |offset| i + offset);
            continue;
        }
        if is_space(ch) || ";&|()<>".contains(ch) {
            if ";&|\n()".contains(ch) {
                assignment_slot = true;
                command_word = true;
                prefix_open = true;
                export_args = false;
                function_name = false;
            }
            i += 1;
            continue;
        }
        let start = i;
        let mut quote: Option<char> = None;
        while i < n {
            let ch = command[i];
            match quote {
                None => {
                    if ch == '"' || ch == '\'' {
                        quote = Some(ch);
                    } else if is_space(ch) || ";&|()<>".contains(ch) {
                        break;
                    }
                }
                Some(open) => {
                    if ch == open {
                        quote = None;
                    }
                }
            }
            i += 1;
        }
        let word = &command[start..i];
        let at_slot = assignment_slot || export_args;
        let keeps = export_args;
        let mut is_command_word = command_word && !is_one_of(word, &TRANSPARENT_BUILTINS);
        let is_function_word = command_word && equals(word, "function");
        if is_function_word {
            // `function NAME { ... }`: the name is no command word.
        } else if function_name {
            is_command_word = false;
        } else if command_word && is_one_of(word, &SHELL_KEYWORDS) {
            // A keyword opens the next command position.
        } else if (assignment_slot || export_args)
            && (LITERAL_ASSIGNMENT.is_full_match(word) || COPIED_ASSIGNMENT.is_full_match(word))
        {
            // An assignment prefix: the command word still follows.
        } else if command_word && is_one_of(word, &TRANSPARENT_BUILTINS) {
            prefix_open = false;
        } else if command_word && is_one_of(word, &EXPORT_COMMANDS) {
            command_word = false;
            prefix_open = false;
            export_args = true;
            assignment_slot = true;
        } else if export_args && word.first() == Some(&'-') {
            // The family's own options (`declare -xi`, `local -r`).
        } else {
            assignment_slot = false;
            command_word = false;
            prefix_open = false;
            export_args = false;
        }
        function_name = is_function_word;
        words.push(ShellWord {
            start,
            end: i,
            assignment: at_slot,
            keeps,
            command: is_command_word,
            open_prefix: prefix_open,
        });
    }
    words
}

/// The word's text when quoting is its only shell syntax, else `None`.
pub(super) fn plain_word_text(word: &[char]) -> Option<String> {
    let haystack = Haystack::from_chars(word);
    let mut content = String::new();
    let n = word.len();
    let mut i = 0;
    while i < n {
        if word[i] == '"' || word[i] == '\'' {
            let close = word[i + 1..].iter().position(|ch| *ch == word[i])? + i + 1;
            let run = &word[i + 1..close];
            if !run.is_empty() && !PLAIN_WORD_RUN.is_full_match(run) {
                return None;
            }
            content.push_str(&string(run));
            i = close + 1;
            continue;
        }
        let found = PLAIN_WORD_RUN.match_at(&haystack, i)?;
        content.push_str(&string(&word[i..found.end()]));
        i = found.end();
    }
    (!content.is_empty()).then_some(content)
}

/// The name a `$NAME`, `${NAME}`, `"$NAME"` word references.
fn referenced_name(word: &[char]) -> Option<String> {
    let reference = VARIABLE_REFERENCE
        .full_match(word)
        .map(|found| (found, word))
        .or_else(|| {
            (word.len() > 2 && word[0] == '"' && word[word.len() - 1] == '"')
                .then(|| &word[1..word.len() - 1])
                .and_then(|inner| {
                    VARIABLE_REFERENCE
                        .full_match(inner)
                        .map(|found| (found, inner))
                })
        })?;
    let (found, text) = reference;
    found.text(text, 1).or_else(|| found.text(text, 2))
}

/// Whether `word` is a variable reference at all (even to an unknown name).
fn is_reference(word: &[char]) -> bool {
    VARIABLE_REFERENCE.is_full_match(word)
        || (word.len() > 2
            && word[0] == '"'
            && word[word.len() - 1] == '"'
            && VARIABLE_REFERENCE.is_full_match(&word[1..word.len() - 1]))
}

/// The command word the shell would execute for `word`, when knowable.
fn revealed_shell_word(word: &[char], assignments: &Names) -> Option<String> {
    if is_reference(word) {
        return referenced_name(word).and_then(|name| assignments.get(&name).cloned());
    }
    plain_word_text(word)
}

/// One `unalias` invocation, parsed the way bash's getopt does: an option it
/// rejects makes the builtin remove nothing.
fn apply_unalias(aliases: &mut Names, words: &[Vec<char>]) {
    let mut clears_all = false;
    let mut names = Vec::new();
    let mut options = true;
    for word in words {
        if options && word.first() == Some(&'-') && !equals(word, "-") {
            if equals(word, "--") {
                options = false;
            } else if equals(word, "-a") {
                clears_all = true;
            } else {
                return;
            }
        } else {
            options = false;
            names.push(word);
        }
    }
    if clears_all {
        aliases.clear();
    }
    for name in names {
        aliases.remove(&plain_word_text(name).unwrap_or_else(|| string(name)));
    }
}

/// True when `text` holds an unquoted separator ending the simple command.
fn separates_commands(text: &[char]) -> bool {
    text.iter().any(|ch| ";&|\n()".contains(*ch))
}

/// The separator nearest one end of `text`: from the end, the one that ends
/// the previous command; from the start, the one that opens the next.
fn segment_separator(text: &[char], from_end: bool) -> Option<&'static str> {
    let indices: Box<dyn Iterator<Item = usize>> = if from_end {
        Box::new((0..text.len()).rev())
    } else {
        Box::new(0..text.len())
    };
    for index in indices {
        let ch = text[index];
        if !";&|\n()".contains(ch) {
            continue;
        }
        // As in the Python guard, a separator at index 0 read from the end
        // looks at the character after it.
        let doubled = if from_end && index > 0 {
            Some(text[index - 1])
        } else {
            text.get(index + 1).copied()
        };
        return Some(match (ch, doubled) {
            ('&', Some('&')) => "&&",
            ('|', Some('|')) => "||",
            (';', _) => ";",
            ('&', _) => "&",
            ('|', _) => "|",
            ('\n', _) => "\n",
            ('(', _) => "(",
            (_, _) => ")",
        });
    }
    None
}

/// Whether a command between these separators runs in the current shell (a
/// pipeline stage, a background command and a `( ... )` group do not).
fn runs_in_current_shell(opens_with: Option<&str>, closes_with: Option<&str>) -> bool {
    let subshell = |separator: Option<&str>| matches!(separator, Some("|" | "&" | "(" | ")"));
    !subshell(opens_with) && !subshell(closes_with)
}

/// The text the walk rebuilt.
pub(super) struct Revealed {
    /// The text with every revealable word replaced by what the shell runs.
    pub text: Vec<char>,
    /// For each character of `text`, its index in the input.
    pub index_map: Vec<usize>,
    /// Input start of each word whose revealed value is more than a bare
    /// executable word.
    pub unnameable: BTreeSet<usize>,
    /// The aliases and assignments the walk ended with.
    pub aliases: Names,
    pub assignments: Names,
    /// For each `eval` command word's position in `text`, the aliases and
    /// assignments live at that eval.
    pub eval_live: BTreeMap<usize, (Names, Names)>,
}

/// How the walk treats alias definitions the text contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AliasReading {
    /// A command word spelled like a defined alias runs its value.
    Expanded,
    /// Command words are read as written.
    AsWritten,
}

/// The first non-`None` capture among groups `from..` of a match.
fn first_value(
    found: &crate::syntax::pyre::Captures,
    text: &[char],
    from: usize,
    to: usize,
) -> String {
    (from..=to)
        .find_map(|group| found.text(text, group))
        .unwrap_or_default()
}

/// Rebuild each shell word the way the shell executes it: quoting stripped,
/// a `$G` reference to an earlier literal assignment replaced by its value, a
/// command word spelled like an alias the text defined replaced by the alias
/// value. Only a word the shell reads as an assignment is recorded, and a
/// command-scoped prefix is dropped again after its command.
#[expect(
    clippy::too_many_lines,
    reason = "one left-to-right walk; its name tables are shared by every arm"
)]
pub(super) fn reveal_shell_command_words(
    command: &[char],
    reading: AliasReading,
    aliases: &Names,
    assignments: &Names,
) -> Revealed {
    let mut assignments = assignments.clone();
    let mut aliases = aliases.clone();
    let mut pending = Names::new();
    let mut alias_args = false;
    let mut unalias_words: Option<Vec<Vec<char>>> = None;
    let mut eval_live = BTreeMap::new();
    let mut out: Vec<char> = Vec::with_capacity(command.len());
    let mut index_map = Vec::with_capacity(command.len());
    let mut unnameable = BTreeSet::new();
    let mut cursor = 0;
    let mut prefix_open = true;
    let mut opened_with: Option<&'static str> = None;
    for word in shell_word_positions(command) {
        let gap = &command[cursor..word.start];
        out.extend_from_slice(gap);
        index_map.extend(cursor..word.start);
        if separates_commands(gap) {
            let closes_with = segment_separator(gap, false);
            if prefix_open {
                assignments.extend(pending.clone());
            }
            pending.clear();
            alias_args = false;
            if let Some(words) = unalias_words.take() {
                if runs_in_current_shell(opened_with, closes_with) {
                    apply_unalias(&mut aliases, &words);
                }
            }
            opened_with = segment_separator(gap, true);
        }
        cursor = word.end;
        let text = &command[word.start..word.end];
        let plain = plain_word_text(text);
        let mut revealed = revealed_shell_word(text, &assignments);
        if reading == AliasReading::Expanded && word.command {
            if let Some(plain) = plain.as_deref().filter(|plain| equals(text, plain)) {
                if let Some(alias) = aliases.get(plain) {
                    revealed = Some(alias.clone());
                }
            }
        }
        let spoken = match &revealed {
            Some(value) => plain_word_text(&value.chars().collect::<Vec<_>>()),
            None => plain.clone(),
        };
        let spoken = spoken.as_deref();
        if word.command && spoken == Some("eval") {
            let mut live = assignments.clone();
            live.extend(pending.clone());
            eval_live.insert(index_map.len(), (aliases.clone(), live));
        } else if word.command && spoken == Some("alias") {
            alias_args = true;
        } else if word.command && spoken == Some("unalias") {
            if let Some(words) = unalias_words.take() {
                apply_unalias(&mut aliases, &words);
            }
            unalias_words = Some(Vec::new());
        } else if let Some(words) = unalias_words.as_mut() {
            words.push(text.to_vec());
        } else if alias_args {
            if let Some(definition) = LITERAL_ASSIGNMENT.full_match(text) {
                let name = definition.text(text, 1).unwrap_or_default();
                aliases.insert(name, first_value(&definition, text, 2, 4));
            } else {
                alias_args = false;
            }
        }
        let replacement: Vec<char> = match &revealed {
            None => text.to_vec(),
            Some(value) => {
                let mut replacement: Vec<char> = value
                    .chars()
                    .map(|ch| if "\"'#\\".contains(ch) { '_' } else { ch })
                    .collect();
                if replacement.len() < text.len() {
                    replacement.resize(text.len(), ' ');
                }
                replacement
            }
        };
        if replacement == text {
            index_map.extend(word.start..word.end);
        } else {
            index_map.extend(std::iter::repeat_n(word.start, replacement.len()));
            if let Some(value) = &revealed {
                if !PLAIN_WORD_RUN.is_full_match(&value.chars().collect::<Vec<_>>()) {
                    unnameable.insert(word.start);
                }
            }
        }
        out.extend_from_slice(&replacement);
        if word.assignment {
            if let Some(assignment) = LITERAL_ASSIGNMENT.full_match(text) {
                let name = assignment.text(text, 1).unwrap_or_default();
                let value = first_value(&assignment, text, 2, 4);
                if word.keeps {
                    pending.remove(&name);
                    assignments.insert(name, value);
                } else {
                    pending.insert(name, value);
                }
            } else if let Some(copied) = COPIED_ASSIGNMENT.full_match(text) {
                let source = copied.text(text, 2).or_else(|| copied.text(text, 3));
                let inherited = source.and_then(|source| {
                    pending
                        .get(&source)
                        .filter(|value| !value.is_empty())
                        .or_else(|| assignments.get(&source))
                        .filter(|value| !value.is_empty())
                        .cloned()
                });
                if let Some(inherited) = inherited {
                    let name = copied.text(text, 1).unwrap_or_default();
                    if word.keeps {
                        assignments.insert(name, inherited);
                    } else {
                        pending.insert(name, inherited);
                    }
                }
            }
        }
        prefix_open = word.open_prefix;
    }
    out.extend_from_slice(&command[cursor..]);
    index_map.extend(cursor..command.len());
    Revealed {
        text: out,
        index_map,
        unnameable,
        aliases,
        assignments,
        eval_live,
    }
}

/// The text the shell runs for one written word: quoting and escapes removed
/// when readable, a reference to a known one-word value substituted, a keyword
/// or wrapper kept, an assignment kept as `N=x`, anything else `x`.
pub(super) fn revealed_word_text(word: &[char], assignments: Option<&Names>) -> String {
    if let Some(plain) = plain_word_text(&strip_shell_escapes(word).0) {
        return plain;
    }
    if let Some(assignments) = assignments.filter(|names| !names.is_empty()) {
        if let Some(value) = referenced_name(word).and_then(|name| assignments.get(&name)) {
            if PLAIN_WORD_RUN.is_full_match(&value.chars().collect::<Vec<_>>()) {
                return value.clone();
            }
        }
    }
    if is_one_of(word, &SHELL_KEYWORDS) || is_one_of(word, &TRANSPARENT_BUILTINS) {
        return string(word);
    }
    if ASSIGNMENT_WORD.is_full_match(word) {
        return "N=x".to_string();
    }
    "x".to_string()
}

/// The written words of `segment`, their revealed text, and the text rebuilt
/// from them with everything between the words left in place.
pub(super) fn revealed_words(
    segment: &[char],
    assignments: Option<&Names>,
) -> (Vec<ShellWord>, Vec<String>, Vec<char>) {
    let written = shell_word_positions(segment);
    let mut revealed = Vec::with_capacity(written.len());
    let mut rebuilt = Vec::with_capacity(segment.len());
    let mut cursor = 0;
    for word in &written {
        rebuilt.extend_from_slice(&segment[cursor..word.start]);
        let text = revealed_word_text(&segment[word.start..word.end], assignments);
        rebuilt.extend(text.chars());
        revealed.push(text);
        cursor = word.end;
    }
    rebuilt.extend_from_slice(&segment[cursor..]);
    (written, revealed, rebuilt)
}

/// `words` with the `command`/`builtin` wrappers and their options dropped.
pub(super) fn builtin_words<'a>(words: &'a [&'a [char]]) -> &'a [&'a [char]] {
    let mut index = 0;
    while index < words.len() {
        let head = revealed_word_text(words[index], None);
        if TRANSPARENT_BUILTINS.contains(&head.as_str()) {
            index += 1;
            while index < words.len() && revealed_word_text(words[index], None).starts_with('-') {
                index += 1;
            }
            continue;
        }
        break;
    }
    &words[index..]
}

/// Whether some word the shell executes in `prefix` is `cd` or `pushd`.
pub(super) fn prefix_holds_directory_command(prefix: &[char]) -> bool {
    let (_, revealed, rebuilt) = revealed_words(prefix, None);
    shell_word_positions(&rebuilt)
        .iter()
        .zip(&revealed)
        .any(|(word, plain)| word.command && (plain == "cd" || plain == "pushd"))
}

#[cfg(test)]
mod tests {
    use super::super::text::chars;
    use super::*;

    #[test]
    fn word_positions_follow_assignments_wrappers_and_keywords() {
        let words = shell_word_positions(&chars("G=git command export H=1; then $G # c"));
        let flags: Vec<(usize, bool, bool, bool)> = words
            .iter()
            .map(|word| (word.start, word.assignment, word.keeps, word.command))
            .collect();
        assert_eq!(
            flags,
            vec![
                (0, true, false, true),
                (6, true, false, false),
                (14, true, false, true),
                (21, true, true, false),
                (26, true, false, true),
                (31, true, false, true),
            ]
        );
    }

    #[test]
    fn revealed_words_read_assignments_and_aliases() {
        let none = Names::new();
        let revealed = reveal_shell_command_words(
            &chars("G='git reset --hard'; $G"),
            AliasReading::Expanded,
            &none,
            &none,
        );
        assert_eq!(
            string(&revealed.text),
            "G='git reset --hard'; git reset --hard"
        );
        assert_eq!(revealed.unnameable, BTreeSet::from([22]));
        assert_eq!(plain_word_text(&chars("g''it")).as_deref(), Some("git"));
        assert_eq!(plain_word_text(&chars("\"a b\"")), None);
    }
}
