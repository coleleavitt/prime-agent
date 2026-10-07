//! The word scan: shell words the way the shell builds argv, with the span
//! each came from and whether it starts a command.

use super::budget::{Budget, Scan};
use super::lexing::{ansi_c_decoded, matching_backtick, matching_paren};
use super::text::{chars, is_space, slice};

/// One shell word: its unquoted argv value plus the span it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellWord {
    pub value: String,
    pub start: usize,
    /// One past the word's last character; an unterminated quote at the end
    /// of the scanned region reports one past the region.
    pub end: usize,
    /// The first word of a fresh (sub)command context.
    pub starts_command: bool,
    /// A command-substitution interior: emitted while the scan was inside a
    /// substitution, before the enclosing word that follows it.
    pub contained: bool,
}

/// Split `command` into shell words.
///
/// Quotes and backslash escapes fold into the word value, comments are
/// skipped, and a command substitution (`$(...)`, backticks) keeps its
/// interior scanned as live commands (one budget unit and one nesting level
/// per interior) while the substitution text itself stays in the enclosing
/// word. A conservative approximation, not a parse: anything it cannot
/// represent exactly ends up refused, never silently allowed.
pub(super) fn scan_words(command: &str, budget: &Budget) -> Scan<Vec<ShellWord>> {
    let text = chars(command);
    scan_word_chars(&text, budget)
}

pub(super) fn scan_word_chars(text: &[char], budget: &Budget) -> Scan<Vec<ShellWord>> {
    let mut scanner = Scanner {
        text,
        words: Vec::new(),
        interior_depth: 0,
        budget,
    };
    scanner.region(0, text.len(), true)?;
    Ok(scanner.words)
}

struct Scanner<'a> {
    text: &'a [char],
    words: Vec<ShellWord>,
    interior_depth: usize,
    budget: &'a Budget,
}

/// The word being built in one region.
struct Pending {
    value: String,
    start: Option<usize>,
    starts_command: bool,
    next_starts_command: bool,
}

impl Scanner<'_> {
    fn interior(&mut self, start: usize, end: usize) -> Scan<()> {
        self.budget.enter()?;
        self.interior_depth += 1;
        let scanned = self.region(start, end, true);
        self.interior_depth -= 1;
        self.budget.leave();
        scanned
    }

    fn flush(&mut self, pending: &mut Pending, end: usize, starts_next_command: bool) {
        if let Some(start) = pending.start.take() {
            self.words.push(ShellWord {
                value: std::mem::take(&mut pending.value),
                start,
                end,
                starts_command: pending.starts_command,
                contained: self.interior_depth > 0,
            });
            pending.next_starts_command = starts_next_command;
        } else {
            pending.next_starts_command |= starts_next_command;
        }
    }

    fn at(&self, index: usize) -> Option<char> {
        self.text.get(index).copied()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one character loop: each arm is a shell quoting rule, read best side by side"
    )]
    fn region(&mut self, start: usize, end: usize, starts_command: bool) -> Scan<()> {
        let text = self.text;
        let mut pending = Pending {
            value: String::new(),
            start: None,
            starts_command: false,
            next_starts_command: starts_command,
        };
        let mut i = start;
        while i < end {
            let ch = text[i];
            if ch == ' ' || ch == '\t' || ch == '\r' {
                self.flush(&mut pending, i, false);
                i += 1;
                continue;
            }
            if "\n;|&()<>".contains(ch) {
                self.flush(&mut pending, i, true);
                i += 1;
                continue;
            }
            if ch == '#' && pending.start.is_none() {
                while i < end && text[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            if pending.start.is_none() {
                pending.start = Some(i);
                pending.starts_command = pending.next_starts_command;
                pending.next_starts_command = false;
            }
            if ch == '\\' && i + 1 < end {
                pending.value.push(text[i + 1]);
                i += 2;
                continue;
            }
            if ch == '$' && self.at(i + 1) == Some('\'') {
                // `$'...'` decodes before argv is built: `$'\x67it'` is `git`.
                let mut j = i + 2;
                let mut body = String::new();
                while j < end {
                    if text[j] == '\\' && self.at(j + 1) == Some('\'') {
                        body.push('\'');
                        j += 2;
                        continue;
                    }
                    if text[j] == '\'' {
                        break;
                    }
                    body.push(text[j]);
                    j += 1;
                }
                pending.value.push_str(&ansi_c_decoded(&body));
                i = j + 1;
                continue;
            }
            if ch == '$' && self.at(i + 1) == Some('"') {
                i += 1; // `$"..."` reads as the double-quoted string after it
                continue;
            }
            if ch == '\'' {
                let mut j = i + 1;
                while j < end && text[j] != '\'' {
                    j += 1;
                }
                pending.value.push_str(&slice(text, i + 1, j));
                i = j + 1;
                continue;
            }
            if ch == '"' {
                let mut j = i + 1;
                while j < end {
                    let inner = text[j];
                    if inner == '\\' && j + 1 < end {
                        pending.value.push(text[j + 1]);
                        j += 2;
                        continue;
                    }
                    if inner == '"' {
                        j += 1;
                        break;
                    }
                    if let Some(close) = self.substitution(j, end)? {
                        pending.value.push_str(&slice(text, j, close + 1));
                        j = close + 1;
                        continue;
                    }
                    pending.value.push(inner);
                    j += 1;
                }
                i = j;
                continue;
            }
            if let Some(close) = self.substitution(i, end)? {
                pending.value.push_str(&slice(text, i, close + 1));
                i = close + 1;
                continue;
            }
            pending.value.push(ch);
            i += 1;
        }
        self.flush(&mut pending, i, false);
        Ok(())
    }

    /// Scan the interior of a substitution opening at `at` (`$(` or a
    /// backtick) and return its closing index; `None` when none opens there.
    fn substitution(&mut self, at: usize, end: usize) -> Scan<Option<usize>> {
        let text = self.text;
        if text[at] == '$' && self.at(at + 1) == Some('(') {
            let close = matching_paren(text, at + 1, end);
            self.interior(at + 2, close)?;
            return Ok(Some(close));
        }
        if text[at] == '`' {
            let close = matching_backtick(text, at, end);
            self.interior(at + 1, close)?;
            return Ok(Some(close));
        }
        Ok(None)
    }
}

/// The argv values of the command that starts at `words[index]`: everything
/// up to the next command boundary, skipping substitution interiors (they
/// run as their own commands, and the enclosing word follows them).
pub(super) fn invocation_tokens(words: &[ShellWord], index: usize) -> Vec<String> {
    let mut tokens = vec![words[index].value.clone()];
    for follower in &words[index + 1..] {
        if follower.starts_command {
            if !follower.contained {
                break;
            }
            continue;
        }
        tokens.push(follower.value.clone());
    }
    tokens
}

/// The command's word values joined by spaces: a payload quoted into one word
/// (`ssh build-box "git push -f origin main"`) shows its text here.
pub(super) fn flattened_text(words: &[ShellWord]) -> String {
    words
        .iter()
        .map(|word| word.value.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// One region split into shell words, quoting-aware. Each word is the literal
/// text the shell would pass, or `None` when it holds something the resolver
/// must refuse to guess at (a substitution or an expansion); the flag is
/// false when the region ended mid-quote. A comment or control character
/// ends the region.
pub(super) fn literal_words(region: &str) -> (Vec<Option<String>>, bool) {
    let text = chars(region);
    let n = text.len();
    let mut words = Vec::new();
    let mut current = String::new();
    let mut unknown = false;
    let mut well_formed = true;
    let flush = |words: &mut Vec<Option<String>>, current: &mut String, unknown: &mut bool| {
        if !current.is_empty() {
            words.push((!*unknown).then(|| current.clone()));
        }
        current.clear();
        *unknown = false;
    };
    let mut i = 0;
    while i < n && well_formed {
        let ch = text[i];
        if is_space(ch) {
            flush(&mut words, &mut current, &mut unknown);
            i += 1;
        } else if ch == '#' {
            break;
        } else if ch == '\'' {
            let Some(close) = text[i + 1..].iter().position(|ch| *ch == '\'') else {
                well_formed = false;
                break;
            };
            let close = i + 1 + close;
            current.push_str(&slice(&text, i + 1, close));
            i = close + 1;
        } else if ch == '"' {
            let mut j = i + 1;
            let mut closed = false;
            while j < n {
                let inner = text[j];
                if inner == '\\' && j + 1 < n {
                    current.push(text[j + 1]);
                    j += 2;
                    continue;
                }
                if inner == '"' {
                    closed = true;
                    break;
                }
                if inner == '$' || inner == '`' {
                    unknown = true;
                }
                current.push(inner);
                j += 1;
            }
            if !closed {
                well_formed = false;
                break;
            }
            i = j + 1;
        } else if ch == '\\' && i + 1 < n {
            current.push(text[i + 1]);
            i += 2;
        } else if ch == '$' || ch == '`' {
            unknown = true;
            current.push(ch);
            i += 1;
        } else if ";&|()<>".contains(ch) {
            break;
        } else {
            current.push(ch);
            i += 1;
        }
    }
    flush(&mut words, &mut current, &mut unknown);
    (words, well_formed)
}

/// Whether every word of a literal split is readable.
pub(super) fn all_literal(words: &[Option<String>]) -> bool {
    words.iter().all(Option::is_some)
}
