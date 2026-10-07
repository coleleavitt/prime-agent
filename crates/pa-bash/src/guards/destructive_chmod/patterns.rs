//! The guard's lexical patterns, written out as matchers over characters
//! (the kernel's regular expressions, with their backtracking and lookaround
//! spelled out where they decide what matches).

use super::pyos::{is_space, is_word_char};

/// A redirection operator matched at one position: `&>`/`&>>`, `>&`, or an
/// fd-prefixed run of one to three `<`/`>` with an optional `&N`
/// duplication, never directly followed by `(` (that is a process
/// substitution). `duplicates` marks the `&N` form, which carries its own
/// target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RedirectOperator {
    pub start: usize,
    pub end: usize,
    pub duplicates: bool,
}

impl RedirectOperator {
    /// Whether the operator text contains `<<<` (a here-string).
    pub(super) fn is_here_string(self, text: &[char]) -> bool {
        text[self.start..self.end]
            .windows(3)
            .any(|w| w == ['<', '<', '<'])
    }

    /// Whether the operator text ends with `<<` (a here-document).
    pub(super) fn is_heredoc(self, text: &[char]) -> bool {
        self.end - self.start >= 2 && text[self.end - 2..self.end] == ['<', '<']
    }
}

/// `_CHMOD_REDIRECT_OPERATOR.match(text, at)`:
/// `(?:&>{1,2}|>&|[0-9]*[<>]{1,3}(&[0-9]+)?)(?!\()`.
pub(super) fn redirect_operator_at(text: &[char], at: usize) -> Option<RedirectOperator> {
    let char_at = |index: usize| text.get(index).copied();
    let not_paren = |end: usize| char_at(end) != Some('(');
    let plain = |end: usize| RedirectOperator {
        start: at,
        end,
        duplicates: false,
    };
    if char_at(at) == Some('&') {
        let arrows = (1..=2)
            .take_while(|offset| char_at(at + offset) == Some('>'))
            .count();
        for count in (1..=arrows).rev() {
            if not_paren(at + 1 + count) {
                return Some(plain(at + 1 + count));
            }
        }
    }
    if char_at(at) == Some('>') && char_at(at + 1) == Some('&') && not_paren(at + 2) {
        return Some(plain(at + 2));
    }
    let digits_end = (at..text.len())
        .find(|index| !text[*index].is_ascii_digit())
        .unwrap_or(text.len());
    let arrows = (0..3)
        .take_while(|offset| matches!(char_at(digits_end + offset), Some('<' | '>')))
        .count();
    for count in (1..=arrows).rev() {
        let after = digits_end + count;
        if char_at(after) == Some('&') {
            let fd_end = (after + 1..text.len())
                .find(|index| !text[*index].is_ascii_digit())
                .unwrap_or(text.len());
            for end in (after + 2..=fd_end).rev() {
                if not_paren(end) {
                    return Some(RedirectOperator {
                        start: at,
                        end,
                        duplicates: true,
                    });
                }
            }
        }
        if not_paren(after) {
            return Some(plain(after));
        }
    }
    None
}

/// `finditer` of the redirection operator over the whole text.
pub(super) fn redirect_operators(text: &[char]) -> Vec<RedirectOperator> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < text.len() {
        match redirect_operator_at(text, at) {
            Some(operator) => {
                at = operator.end;
                found.push(operator);
            }
            None => at += 1,
        }
    }
    found
}

/// `_CHMOD_STATIC_REDIRECT_TARGET.match(text, at).end()`: the end of a run
/// of characters that are neither whitespace nor one of ``;&|<>()$`"'``.
pub(super) fn static_target_end(text: &[char], at: usize) -> usize {
    (at..text.len())
        .find(|index| {
            let c = text[*index];
            is_space(c) || ";&|<>()$`\"'".contains(c)
        })
        .unwrap_or(text.len())
}

/// `[\s;&|(){}]` (the characters a comment `#` may follow).
pub(super) fn opens_comment_after(c: char) -> bool {
    is_space(c) || ";&|(){}".contains(c)
}

/// Any of ``$`*?{}[]`` in the text (`_CHMOD_GLOB_OR_SUBSTITUTION`).
pub(super) fn has_glob_or_substitution(text: &str) -> bool {
    text.contains(['$', '`', '*', '?', '{', '}', '[', ']'])
}

/// A `$` or a backtick in the text (`_UNRESOLVED_EXPANSION`).
pub(super) fn has_unresolved_expansion(text: &str) -> bool {
    text.contains(['$', '`'])
}

/// `[*?{\[]` anywhere in the text.
pub(super) fn has_expandable_glob(text: &str) -> bool {
    text.contains(['*', '?', '{', '['])
}

/// `^[A-Za-z_][A-Za-z0-9_]*\+?=`: a plain or append assignment word.
pub(super) fn is_assignment_word(value: &str) -> bool {
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return false;
    }
    let rest: String = chars.collect();
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    let tail = &rest[name_end..];
    tail.starts_with('=') || tail.starts_with("+=")
}

/// `^NAME\+?=` for one fixed name.
pub(super) fn assigns(value: &str, name: &str) -> bool {
    value
        .strip_prefix(name)
        .is_some_and(|tail| tail.starts_with('=') || tail.starts_with("+="))
}

/// `(?<![A-Za-z0-9_])NAME\+?=` anywhere in the text.
pub(super) fn assigns_anywhere(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        let before_ok = text[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        let tail = &text[at + name.len()..];
        before_ok && (tail.starts_with('=') || tail.starts_with("+="))
    })
}

/// `\b(?:w1|w2|...)\b` anywhere in the text (Unicode word boundaries).
pub(super) fn has_word(text: &str, words: &[&str]) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|at| {
        if at > 0 && is_word_char(chars[at - 1]) {
            return false;
        }
        words.iter().any(|word| {
            let word: Vec<char> = word.chars().collect();
            let end = at + word.len();
            end <= chars.len()
                && chars[at..end] == word[..]
                && chars.get(end).is_none_or(|c| !is_word_char(*c))
        })
    })
}

/// `\(\s*\)\s*[({]|function\s+[A-Za-z_]` anywhere in the text.
pub(super) fn has_function_definition(text: &[char]) -> bool {
    let skip_space = |mut at: usize| {
        while at < text.len() && is_space(text[at]) {
            at += 1;
        }
        at
    };
    (0..text.len()).any(|at| {
        if text[at] == '(' {
            let close = skip_space(at + 1);
            if text.get(close) == Some(&')') {
                let body = skip_space(close + 1);
                if matches!(text.get(body), Some('(' | '{')) {
                    return true;
                }
            }
        }
        let keyword: [char; 8] = ['f', 'u', 'n', 'c', 't', 'i', 'o', 'n'];
        if text.len() >= at + 8 && text[at..at + 8] == keyword {
            let name = skip_space(at + 8);
            if name > at + 8
                && text
                    .get(name)
                    .is_some_and(|c| c.is_ascii_alphabetic() || *c == '_')
            {
                return true;
            }
        }
        false
    })
}

/// `re.split(r"[;&|\n]", text, maxsplit=1)[0]`.
pub(super) fn before_separator(text: &[char]) -> &[char] {
    let end = text
        .iter()
        .position(|c| matches!(c, ';' | '&' | '|' | '\n'))
        .unwrap_or(text.len());
    &text[..end]
}

/// `re.finditer(r"<>?\s*([^\s;&|<>()]+)", region)`: the targets of the
/// region's stdin redirections.
pub(super) fn stdin_redirect_targets(region: &[char]) -> Vec<String> {
    let is_target = |c: char| !(is_space(c) || ";&|<>()".contains(c));
    let mut targets = Vec::new();
    let mut at = 0;
    while at < region.len() {
        if region[at] != '<' {
            at += 1;
            continue;
        }
        let attempt = |after_op: usize| -> Option<(usize, usize)> {
            let mut start = after_op;
            while start < region.len() && is_space(region[start]) {
                start += 1;
            }
            let end = (start..region.len())
                .find(|index| !is_target(region[*index]))
                .unwrap_or(region.len());
            (end > start).then_some((start, end))
        };
        let matched = if region.get(at + 1) == Some(&'>') {
            attempt(at + 2).or_else(|| attempt(at + 1))
        } else {
            attempt(at + 1)
        };
        match matched {
            Some((start, end)) => {
                targets.push(region[start..end].iter().collect());
                at = end;
            }
            None => at += 1,
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn redirect_operators_backtrack_like_the_regex() {
        let cases: [(&str, Option<(usize, bool)>); 9] = [
            ("2>/dev/null", Some((2, false))),
            ("&>>x", Some((3, false))),
            ("&>>(", Some((2, false))),
            (">&2", Some((2, false))),
            ("<(", None),
            ("<<(", Some((1, false))),
            ("2>&1", Some((4, true))),
            ("<<<x", Some((3, false))),
            ("x", None),
        ];
        for (text, expected) in cases {
            let found = redirect_operator_at(&chars(text), 0).map(|op| (op.end, op.duplicates));
            assert_eq!(found, expected, "{text}");
        }
    }

    #[test]
    fn word_boundaries_and_assignments() {
        assert!(has_word("a && cd x", &["cd", "pushd"]));
        assert!(!has_word("abcd x", &["cd"]));
        assert!(is_assignment_word("PATH+=x"));
        assert!(!is_assignment_word("1A=x"));
        assert!(assigns_anywhere("x;CDPATH+=/", "CDPATH"));
        assert!(!assigns_anywhere("XCDPATH=/", "CDPATH"));
        assert_eq!(
            stdin_redirect_targets(&chars("bash <<EOF < s.sh")),
            ["EOF", "s.sh"]
        );
    }
}
