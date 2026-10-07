//! The command substitutions a command runs: each `$(...)` and backtick
//! interior is scanned as a command of its own.

use super::mask::{paren_matches, quoted_span_end, Closer, WORD_BREAKERS};

/// Whether an unmatched opener inside an interior yields another tail to
/// scan. An unmatched `$(` runs to the end of the command, so every unmatched
/// opener inside its tail is a suffix of the same tail: returning that tail
/// once keeps a command of unterminated openers to one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Descend {
    Yes,
    No,
}

/// The interior of every `$(...)` and backtick substitution in `command`.
///
/// Single-quoted and ANSI-C spans, comments and escaped characters never
/// start one; a double quote keeps its substitutions live. `$((` with adjacent
/// closers is arithmetic and queues nothing, but the walk steps over its `$(`
/// alone so a substitution inside it still counts (`echo $(( $(env) ))`).
/// Only the outermost span of a nest is queued: its interior carries the rest.
pub(super) fn command_substitutions(
    command: &[char],
    descend: Descend,
) -> Vec<(Vec<char>, Descend)> {
    let mut interiors = Vec::new();
    let matches = paren_matches(command);
    let n = command.len();
    let mut index = 0;
    let mut in_word = false;
    let mut in_double_quotes = false;
    while index < n {
        let ch = command[index];
        let next = command.get(index + 1).copied();
        if ch == '\\' {
            in_word = true;
            index += 2;
            continue;
        }
        if ch == '"' {
            in_double_quotes = !in_double_quotes;
            in_word = true;
            index += 1;
            continue;
        }
        if !in_double_quotes {
            if ch == '\'' || (ch == '$' && next == Some('\'')) {
                in_word = true;
                let quote_index = if ch == '\'' { index } else { index + 1 };
                // An unterminated span is not trusted to hide anything.
                index = quoted_span_end(command, quote_index).unwrap_or(index + 1);
                continue;
            }
            if ch == '#' && !in_word {
                while index < n && command[index] != '\n' {
                    index += 1;
                }
                continue;
            }
        }
        in_word = true;
        if ch == '$' && next == Some('(') {
            if command.get(index + 2) == Some(&'(') && matches.is_arithmetic(index) {
                index += 2;
                continue;
            }
            match matches.closer(index + 1) {
                Closer::At(close) => {
                    interiors.push((command[index + 2..close].to_vec(), Descend::Yes));
                    index = close + 1;
                }
                Closer::Missing | Closer::Unmatched => match descend {
                    Descend::Yes => {
                        interiors.push((command[index + 2..].to_vec(), Descend::No));
                        index = n;
                    }
                    Descend::No => index += 2,
                },
            }
            continue;
        }
        if ch == '`' {
            let mut end = index + 1;
            while end < n {
                if command[end] == '\\' {
                    end += 2;
                    continue;
                }
                if command[end] == '`' {
                    break;
                }
                end += 1;
            }
            if end < n {
                interiors.push((command[index + 1..end].to_vec(), Descend::Yes));
                index = end + 1;
            } else {
                interiors.push((command[index + 1..end.min(n)].to_vec(), Descend::No));
                index = n;
            }
            continue;
        }
        if WORD_BREAKERS.contains(ch) {
            in_word = false;
        }
        index += 1;
    }
    interiors
}
