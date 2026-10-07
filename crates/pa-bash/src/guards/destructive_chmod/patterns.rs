//! The guard's lexical patterns, written out as matchers over characters
//! (the kernel's regular expressions, with their backtracking and lookaround
//! spelled out where they decide what matches).

use std::sync::LazyLock;

use crate::syntax::chars::is_space;
use crate::syntax::pyre::{Captures, Haystack, PyRegex};

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

/// `(?:&>{1,2}|>&|[0-9]*[<>]{1,3}(&[0-9]+)?)(?!\()`, with the lookahead
/// spelled as one consumed character (`[^(]` or the end) after the operator
/// group: the same backtracking order without lookaround, which keeps the
/// pattern on the linear engine (an anchored lookaround pattern rescans the
/// rest of the text at every masking position).
static REDIRECT_OPERATOR: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"((?:&>{1,2}|>&|[0-9]*[<>]{1,3}(&[0-9]+)?))(?:[^(]|\z)"));

fn operator(found: &Captures) -> RedirectOperator {
    let (start, end) = found.group(1).unwrap_or((found.start(), found.start()));
    RedirectOperator {
        start,
        end,
        duplicates: found.group(2).is_some(),
    }
}

/// `_CHMOD_REDIRECT_OPERATOR.match(text, at)`: `&>`/`&>>`, `>&`, or an
/// fd-prefixed run of one to three `<`/`>` with an optional `&N`, never
/// directly followed by `(` (a process substitution).
pub(super) fn redirect_operator_at(text: &Haystack, at: usize) -> Option<RedirectOperator> {
    REDIRECT_OPERATOR.match_at(text, at).as_ref().map(operator)
}

/// `finditer` of the redirection operator over the whole text.
pub(super) fn redirect_operators(text: &[char]) -> Vec<RedirectOperator> {
    let haystack = Haystack::from_chars(text);
    let mut found = Vec::new();
    let mut at = 0;
    // Resume at the operator's end, not the match end: the consumed
    // lookahead character may start the next operator.
    while let Some(next) = REDIRECT_OPERATOR.search_from(&haystack, at) {
        let next = operator(&next);
        at = next.end;
        found.push(next);
    }
    found
}

/// `_CHMOD_STATIC_REDIRECT_TARGET.match(text, at).end()`: the end of the run
/// of characters in ``[^\s;&|<>()$`"']``.
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

static ASSIGNMENT_WORD: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"^[A-Za-z_][A-Za-z0-9_]*\+?="));
static CDPATH_ASSIGNMENT: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?<![A-Za-z0-9_])CDPATH\+?="));
static PATH_ASSIGNMENT: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"(?<![A-Za-z0-9_])PATH\+?="));
static FUNCTION_DEFINITION: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"\(\s*\)\s*[({]|function\s+[A-Za-z_]"));
static STDIN_REDIRECT_TARGET: LazyLock<PyRegex> =
    LazyLock::new(|| PyRegex::new(r"<>?\s*([^\s;&|<>()]+)"));

/// `^[A-Za-z_][A-Za-z0-9_]*\+?=`: a plain or append assignment word.
pub(super) fn is_assignment_word(value: &str) -> bool {
    ASSIGNMENT_WORD.is_found(value)
}

/// `^NAME\+?=` for one fixed name.
pub(super) fn assigns(value: &str, name: &str) -> bool {
    value
        .strip_prefix(name)
        .is_some_and(|tail| tail.starts_with('=') || tail.starts_with("+="))
}

/// A variable whose assignment anywhere in a text changes how the guard
/// resolves operands (`(?<![A-Za-z0-9_])NAME\+?=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Assigned {
    Cdpath,
    Path,
}

/// Whether `text` assigns `variable` anywhere.
pub(super) fn assigns_anywhere(text: &str, variable: Assigned) -> bool {
    match variable {
        Assigned::Cdpath => CDPATH_ASSIGNMENT.is_found(text),
        Assigned::Path => PATH_ASSIGNMENT.is_found(text),
    }
}

/// `\(\s*\)\s*[({]|function\s+[A-Za-z_]` anywhere in the text.
pub(super) fn has_function_definition(text: &[char]) -> bool {
    FUNCTION_DEFINITION.is_found(text)
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
    STDIN_REDIRECT_TARGET
        .find_all(region)
        .iter()
        .filter_map(|found| found.text(region, 1))
        .collect()
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
            let found =
                redirect_operator_at(&Haystack::new(text), 0).map(|op| (op.end, op.duplicates));
            assert_eq!(found, expected, "{text}");
        }
    }

    #[test]
    fn word_boundaries_and_assignments() {
        assert!(is_assignment_word("PATH+=x"));
        assert!(!is_assignment_word("1A=x"));
        assert!(assigns_anywhere("x;CDPATH+=/", Assigned::Cdpath));
        assert!(!assigns_anywhere("XCDPATH=/", Assigned::Cdpath));
        assert_eq!(
            stdin_redirect_targets(&chars("bash <<EOF < s.sh")),
            ["EOF", "s.sh"]
        );
    }
}
