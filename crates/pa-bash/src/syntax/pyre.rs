//! The guards' regular expressions, read the way Python's `re` reads them.
//!
//! The guards keep the Python patterns in their original spelling and run
//! them on `fancy-regex`, after a small source translation for the places the
//! two dialects disagree on the patterns the guards use:
//!
//! - `\s` / `\S`: Python's `str.isspace()` also counts `\x1c`-`\x1f`;
//! - `$`: Python's end anchor also matches before a final `\n`;
//! - inside a class, `[`, `&` and `~` are literals in Python (the regex crate
//!   reads `[` as a nested class and `&&`/`~~` as set operations).
//!
//! Positions are `char` indices (Python string indices), not byte offsets.

use std::borrow::Cow;

use fancy_regex::Regex;

/// Text a pattern can be matched against whole: `&str` or the guards'
/// `&[char]` buffers.
pub(crate) trait AsText {
    fn as_text(&self) -> Cow<'_, str>;
}

impl AsText for str {
    fn as_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(self)
    }
}

impl AsText for String {
    fn as_text(&self) -> Cow<'_, str> {
        Cow::Borrowed(self)
    }
}

impl AsText for [char] {
    fn as_text(&self) -> Cow<'_, str> {
        Cow::Owned(self.iter().collect())
    }
}

impl<T: AsText + ?Sized> AsText for &T {
    fn as_text(&self) -> Cow<'_, str> {
        (**self).as_text()
    }
}

impl AsText for Vec<char> {
    fn as_text(&self) -> Cow<'_, str> {
        Cow::Owned(self.iter().collect())
    }
}

/// Bound on the backtracking VM's steps per search; far above what the
/// guards' patterns need on 64 KiB inputs (exceeding it is reported as no
/// match, the direction every caller treats as "not this shape").
const BACKTRACK_LIMIT: usize = 50_000_000;

/// Python's `\w` for `str` patterns: `str.isalnum()` (letters and numbers
/// of every kind) or `_`. The regex crate's `\w` also takes combining marks
/// and connector punctuation, which would move word boundaries.
const WORD_MEMBERS: &str = r"\p{L}\p{N}_";
const WORD: &str = r"[\p{L}\p{N}_]";
const NOT_WORD: &str = r"[^\p{L}\p{N}_]";
/// `\b` / `\B` over that `\w`.
const WORD_BOUNDARY: &str =
    r"(?:(?<=[\p{L}\p{N}_])(?![\p{L}\p{N}_])|(?<![\p{L}\p{N}_])(?=[\p{L}\p{N}_]))";
const NOT_WORD_BOUNDARY: &str =
    r"(?:(?<=[\p{L}\p{N}_])(?=[\p{L}\p{N}_])|(?<![\p{L}\p{N}_])(?![\p{L}\p{N}_]))";

/// Rewrite a Python pattern into the `fancy-regex` dialect.
fn translate(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len() + 16);
    let mut in_class = false;
    let mut class_start = 0usize;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            match (in_class, next) {
                (false, 's') => out.push_str(r"[\s\x1c-\x1f]"),
                (false, 'S') => out.push_str(r"[^\s\x1c-\x1f]"),
                (true, 's') => out.push_str(r"\s\x1c-\x1f"),
                (false, 'Z') => out.push_str(r"\z"),
                (false, 'w') => out.push_str(WORD),
                (false, 'W') => out.push_str(NOT_WORD),
                (true, 'w') => out.push_str(WORD_MEMBERS),
                (false, 'b') => out.push_str(WORD_BOUNDARY),
                (false, 'B') => out.push_str(NOT_WORD_BOUNDARY),
                _ => {
                    out.push('\\');
                    out.push(next);
                }
            }
            i += 2;
            continue;
        }
        if in_class {
            // A `]` right after `[` or `[^` is a literal member.
            let first = i == class_start;
            match ch {
                ']' if !first => in_class = false,
                ']' | '[' | '&' | '~' => {
                    out.push('\\');
                    out.push(ch);
                    i += 1;
                    continue;
                }
                _ => {}
            }
            out.push(ch);
            i += 1;
            continue;
        }
        match ch {
            '[' => {
                in_class = true;
                out.push('[');
                i += 1;
                if chars.get(i) == Some(&'^') {
                    out.push('^');
                    i += 1;
                }
                class_start = i;
                continue;
            }
            '$' => out.push_str(r"(?:\z|(?=\n\z))"),
            _ => out.push(ch),
        }
        i += 1;
    }
    out
}

fn build(source: &str) -> Result<Regex, Box<fancy_regex::Error>> {
    fancy_regex::RegexBuilder::new(source)
        .backtrack_limit(BACKTRACK_LIMIT)
        .build()
        .map_err(Box::new)
}

/// One compiled Python pattern.
#[derive(Debug)]
pub(crate) struct PyRegex {
    search: Regex,
    /// `\A(?:...)`, for anchored matches on a suffix of the text; `None` when
    /// the pattern looks at what precedes the match (`\b`, lookbehind), which
    /// a suffix would hide.
    anchored: Option<Regex>,
    full: Regex,
    /// Literals one of which every match contains (ASCII case-insensitively
    /// when `literals_any_case`): a text holding none of them cannot match,
    /// which spares the backtracking engine a fruitless walk of a long text.
    literals: &'static [&'static str],
    literals_any_case: bool,
}

impl PyRegex {
    /// Compile one of the guards' fixed patterns.
    ///
    /// # Panics
    ///
    /// When `source` is not a valid pattern: the patterns are constants.
    pub(crate) fn new(source: &str) -> Self {
        Self::compile(source)
            .unwrap_or_else(|error| panic!("invalid guard pattern {source:?}: {error}"))
    }

    /// Compile a pattern built at run time.
    pub(crate) fn compile(source: &str) -> Result<Self, Box<fancy_regex::Error>> {
        let translated = translate(source);
        let looks_back = source.contains("\\b") || source.contains("(?<") || source.contains("\\B");
        Ok(Self {
            search: build(&translated)?,
            anchored: if looks_back {
                None
            } else {
                Some(build(&format!(r"\A(?:{translated})"))?)
            },
            full: build(&format!(r"\A(?:{translated})\z"))?,
            literals: &[],
            literals_any_case: false,
        })
    }

    /// Declare literals one of which every match contains (a prefilter: the
    /// caller vouches for it from the pattern's text).
    #[must_use]
    pub(crate) fn requiring(mut self, literals: &'static [&'static str]) -> Self {
        self.literals = literals;
        self
    }

    /// [`Self::requiring`], with the literals compared ASCII
    /// case-insensitively (for `(?i)` patterns).
    #[must_use]
    pub(crate) fn requiring_any_case(mut self, literals: &'static [&'static str]) -> Self {
        self.literals = literals;
        self.literals_any_case = true;
        self
    }

    fn may_match(&self, text: &str) -> bool {
        if self.literals.is_empty() {
            return true;
        }
        if self.literals_any_case {
            // `(?i)` folds some non-ASCII letters onto ASCII ones (`ſ` is `s`,
            // the Kelvin sign is `k`): only an ASCII text can be ruled out by
            // an ASCII comparison.
            if !text.is_ascii() {
                return true;
            }
            let lowered = text.to_ascii_lowercase();
            self.literals
                .iter()
                .any(|literal| lowered.contains(literal))
        } else {
            self.literals.iter().any(|literal| text.contains(literal))
        }
    }

    /// `re.match(pattern, text, pos)`: a match starting exactly at `start`.
    pub(crate) fn match_at(&self, text: &Haystack, start: usize) -> Option<Captures> {
        let at = text.byte(start)?;
        if let Some(anchored) = &self.anchored {
            let found = anchored.captures(&text.text[at..]).ok().flatten()?;
            return Some(text.captures(&found, at));
        }
        let found = self
            .search
            .captures_from_pos(&text.text, at)
            .ok()
            .flatten()?;
        (found.get(0)?.start() == at).then(|| text.captures(&found, 0))
    }

    /// `re.fullmatch`: the whole text.
    pub(crate) fn full_match(&self, text: &(impl AsText + ?Sized)) -> Option<Captures> {
        let text = text.as_text();
        let haystack = Haystack::new(&text);
        let found = self.full.captures(&text).ok().flatten()?;
        Some(haystack.captures(&found, 0))
    }

    /// Whether `text` is a full match.
    pub(crate) fn is_full_match(&self, text: &(impl AsText + ?Sized)) -> bool {
        self.full.is_match(&text.as_text()).unwrap_or(false)
    }

    /// `re.search` from `start`.
    pub(crate) fn search_from(&self, text: &Haystack, start: usize) -> Option<Captures> {
        let at = text.byte(start)?;
        let found = self
            .search
            .captures_from_pos(&text.text, at)
            .ok()
            .flatten()?;
        Some(text.captures(&found, 0))
    }

    /// Whether `re.search` finds a match.
    pub(crate) fn is_found(&self, text: &(impl AsText + ?Sized)) -> bool {
        let text = text.as_text();
        self.may_match(&text) && self.search.is_match(&text).unwrap_or(false)
    }

    /// `re.match(pattern, text)`: a match at the start of the text.
    pub(crate) fn match_start(&self, text: &(impl AsText + ?Sized)) -> Option<Captures> {
        self.match_at(&Haystack::new(&text.as_text()), 0)
    }

    /// `re.finditer`: non-overlapping matches, left to right.
    pub(crate) fn find_all(&self, text: &(impl AsText + ?Sized)) -> Vec<Captures> {
        let text = text.as_text();
        if !self.may_match(&text) {
            return Vec::new();
        }
        self.find_all_in(&Haystack::new(&text))
    }

    fn find_all_in(&self, text: &Haystack) -> Vec<Captures> {
        let mut found = Vec::new();
        let mut at = 0;
        while at <= text.len() {
            let Some(captures) = self.search_from(text, at) else {
                break;
            };
            at = if captures.end() == captures.start() {
                captures.end() + 1
            } else {
                captures.end()
            };
            found.push(captures);
        }
        found
    }

    /// `re.split` with one capturing group around the whole separator:
    /// pieces and separators alternate (`[piece, sep, piece, ...]`).
    pub(crate) fn split_keeping(&self, text: &(impl AsText + ?Sized)) -> Vec<Vec<char>> {
        let text = Haystack::new(&text.as_text());
        let chars =
            |start: usize, end: usize| text.slice(start, end).chars().collect::<Vec<char>>();
        let mut parts = Vec::new();
        let mut cursor = 0;
        for captures in self.find_all_in(&text) {
            parts.push(chars(cursor, captures.start()));
            parts.push(chars(captures.start(), captures.end()));
            cursor = captures.end();
        }
        parts.push(chars(cursor, text.len()));
        parts
    }
}

/// A text prepared for searching by `char` index.
#[derive(Debug, Clone)]
pub(crate) struct Haystack {
    text: String,
    /// The byte offset of every char, plus the text length.
    offsets: Vec<usize>,
}

impl Haystack {
    pub(crate) fn new(text: &str) -> Self {
        let mut offsets: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        offsets.push(text.len());
        Self {
            text: text.to_string(),
            offsets,
        }
    }

    pub(crate) fn from_chars(chars: &[char]) -> Self {
        Self::new(&chars.iter().collect::<String>())
    }

    /// The length in chars.
    pub(crate) fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    fn byte(&self, index: usize) -> Option<usize> {
        self.offsets.get(index).copied()
    }

    fn char_index(&self, byte: usize) -> usize {
        self.offsets.partition_point(|&at| at < byte)
    }

    /// The text between two char indices.
    pub(crate) fn slice(&self, start: usize, end: usize) -> &str {
        &self.text[self.offsets[start]..self.offsets[end]]
    }

    fn captures(&self, found: &fancy_regex::Captures<'_>, base: usize) -> Captures {
        Captures {
            spans: (0..found.len())
                .map(|group| {
                    found.get(group).map(|span| {
                        (
                            self.char_index(base + span.start()),
                            self.char_index(base + span.end()),
                        )
                    })
                })
                .collect(),
        }
    }
}

/// One match: the char span of the whole match and of each capturing group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Captures {
    spans: Vec<Option<(usize, usize)>>,
}

impl Captures {
    pub(crate) fn start(&self) -> usize {
        self.spans[0].map_or(0, |(start, _)| start)
    }

    pub(crate) fn end(&self) -> usize {
        self.spans[0].map_or(0, |(_, end)| end)
    }

    /// The span of group `index` when it took part in the match.
    pub(crate) fn group(&self, index: usize) -> Option<(usize, usize)> {
        self.spans.get(index).copied().flatten()
    }

    /// The text of group `index` when it took part in the match.
    pub(crate) fn text(&self, text: &[char], index: usize) -> Option<String> {
        self.group(index)
            .map(|(start, end)| text[start..end].iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternation_backtracks_in_python_order() {
        let value = PyRegex::new(r#"(?:"[^"]*"|[^-\s;&|][^\s;&|]*)\s+x"#);
        // The quoted alternative fails at `\s+`, the run alternative takes over.
        assert_eq!(
            value
                .match_at(&Haystack::new(r#""a b"c x"#), 0)
                .map(|c| c.end()),
            None
        );
        assert_eq!(
            value
                .match_at(&Haystack::new(r#""a b" x"#), 0)
                .map(|c| c.end()),
            Some(7)
        );
        let full = PyRegex::new(r"([A-Za-z_][A-Za-z0-9_]*)=(?:'([^']*)'|([A-Za-z0-9_./-]+))");
        let captures = full.full_match("G='git reset'").expect("full match");
        assert_eq!(
            (captures.group(1), captures.group(2), captures.group(3)),
            (Some((0, 1)), Some((3, 12)), None)
        );
    }

    #[test]
    fn end_anchor_and_space_follow_python() {
        let config = PyRegex::new(r"core\.(worktree|bare)(=|$)");
        assert!(config.match_at(&Haystack::new("core.bare"), 0).is_some());
        assert!(config.match_at(&Haystack::new("core.bare\n"), 0).is_some());
        assert!(config.match_at(&Haystack::new("core.bareX"), 0).is_none());
        assert!(PyRegex::new(r"a\sb").is_full_match("a\u{1f}b"));
        assert!(PyRegex::new(r"[^\s]").is_full_match("é"));
    }

    #[test]
    fn word_classes_follow_python() {
        // A combining mark is not `str.isalnum()`: Python sees a boundary.
        assert!(PyRegex::new(r"\bgit\b").is_found("\u{301}git"));
        assert!(PyRegex::new(r"x(?![\w.-])").is_found("x\u{301}"));
        assert!(!PyRegex::new(r"\bgit\b").is_found("égit"));
        assert!(PyRegex::new(r"\w+").is_full_match("a½_٣"));
    }

    #[test]
    fn word_boundaries_see_the_preceding_text() {
        let pattern = PyRegex::new(r"\bgit(?=\s|$|[;&|)])");
        let text = "xgit git; gït git\n";
        let starts: Vec<usize> = pattern.find_all(text).iter().map(Captures::start).collect();
        assert_eq!(starts, vec![5, 14]);
        assert!(pattern.match_at(&Haystack::new(text), 1).is_none());
    }

    #[test]
    fn split_keeps_separators() {
        let split = PyRegex::new(r"(&&|\|\||;|\||\n)");
        let parts: Vec<String> = split
            .split_keeping("a&&b|c")
            .iter()
            .map(|part| part.iter().collect())
            .collect();
        assert_eq!(parts, vec!["a", "&&", "b", "|", "c"]);
    }

    #[test]
    fn class_literals_stay_literal() {
        assert!(PyRegex::new(r"[[&~]+").is_full_match("[&~"));
        assert!(PyRegex::new(r"[]a]").is_full_match("]"));
    }
}
