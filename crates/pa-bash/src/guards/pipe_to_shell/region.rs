//! The region scanner: one quote-aware pass that splits shell text into
//! pipeline stages and the words each stage's argv holds.
//!
//! Quotes fold into the words they build (`"curl"` and `cu"rl"` both run
//! curl), a backslash-newline continuation joins the words it splits, ANSI-C
//! `$'...'` quoting builds its word literally, a `#` at a word boundary starts
//! a comment, and a redirection is consumed with its target word because the
//! shell takes both out of the argv. A command substitution keeps its text in
//! the enclosing word -- so a word carrying one never resolves -- while its
//! interior span is reported for the caller to scan as live commands. This is a
//! conservative approximation, not a parse: what it cannot represent exactly is
//! reported as unresolvable, never silently allowed.
//!
//! All positions are `char` indices into the scanned text.

/// A `[start, end)` range of `char` indices.
pub(super) type Span = (usize, usize);

/// One shell word: the value the shell would build for it, the command
/// substitutions it carries, and whether the scan could resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Word {
    pub value: String,
    pub substitutions: Vec<Span>,
    pub resolvable: bool,
    /// Whether quote characters built this word: a quoted word is data the
    /// shell passed through, never a reserved word (`"{"` runs a program
    /// named `{`).
    pub quoted: bool,
}

impl Word {
    /// A literal word built from quoted text (an `env -S` operand).
    pub(super) fn quoted_literal(value: String) -> Self {
        Self {
            value,
            substitutions: Vec::new(),
            resolvable: true,
            quoted: true,
        }
    }

    /// Whether this is the unquoted reserved word `word`.
    pub(super) fn is_bare(&self, word: &str) -> bool {
        !self.quoted && self.value == word
    }
}

/// The operator that ended a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Separator {
    /// The end of the region.
    End,
    Newline,
    Semicolon,
    /// `&` (background).
    Background,
    And,
    Or,
    /// `|`.
    Pipe,
    /// `|&`: stderr and stdout both reach the next stage.
    PipeBoth,
    OpenParen,
    CloseParen,
}

impl Separator {
    /// Operators that join the stages of one pipeline.
    pub(super) fn is_pipe(self) -> bool {
        matches!(self, Separator::Pipe | Separator::PipeBoth)
    }

    /// Operators that group commands without ending a pipeline
    /// (`(curl URL) | sh` still pipes the download into the interpreter).
    pub(super) fn is_grouping(self) -> bool {
        matches!(self, Separator::OpenParen | Separator::CloseParen)
    }

    /// Separators that end one statement, not just one stage.
    pub(super) fn ends_statement(self) -> bool {
        matches!(
            self,
            Separator::Semicolon
                | Separator::Newline
                | Separator::Background
                | Separator::And
                | Separator::Or
        )
    }
}

/// A here-document body feeding a stage, and whether its delimiter was
/// quoted (an unquoted body's substitutions expand at read time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HeredocBody {
    pub span: Span,
    pub quoted: bool,
}

/// One command stage: the operator that ended it, its words, the
/// substitutions a redirection took out of those words (the shell consumes
/// the redirection, but its substitution still runs), and the
/// here-document bodies that feed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Stage {
    pub separator: Separator,
    pub words: Vec<Word>,
    pub target_substitutions: Vec<Span>,
    pub heredoc_bodies: Vec<HeredocBody>,
}

/// One scanned region: its stages, the substitution interiors inside them,
/// and whether an unterminated quote left the region unresolvable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Region {
    pub stages: Vec<Stage>,
    pub substitutions: Vec<Span>,
    pub unterminated_quote: bool,
}

/// Every character that ends a command stage.
const SEPARATORS: [char; 6] = ['\n', ';', '|', '&', '(', ')'];

fn is_separator(char: char) -> bool {
    SEPARATORS.contains(&char)
}

fn is_redirect_char(char: char) -> bool {
    matches!(char, '<' | '>')
}

/// `text[index]`, or `None` past the end of the text.
fn at(text: &[char], index: usize) -> Option<char> {
    text.get(index).copied()
}

/// `text[start..end]` with Python's slice clamping.
fn slice(text: &[char], start: usize, end: usize) -> &[char] {
    let end = end.min(text.len());
    &text[start.min(end)..end]
}

/// Index just past the quoted span starting at `text[start]` (a single or
/// double quote), skipping escaped characters inside double quotes.
fn quote_span_end(text: &[char], start: usize, end: usize) -> usize {
    let quote = text[start];
    let mut i = start + 1;
    while i < end {
        let char = text[i];
        if quote == '"' && char == '\\' {
            i += 2;
            continue;
        }
        if char == quote {
            return i + 1;
        }
        i += 1;
    }
    end
}

/// Index of the `)` matching the `(` at `open`, or `end - 1`.
///
/// Quote-aware: a `)` inside a single- or double-quoted span or after a
/// backslash never closes the substitution. Unterminated quotes or an
/// unmatched `(` scan to the end, so the whole region stays live-command
/// territory rather than a miss.
pub(super) fn matching_paren(text: &[char], open: usize, end: usize) -> usize {
    let mut depth = 0isize;
    let mut i = open;
    while i < end {
        match text[i] {
            '\\' => i += 2,
            '\'' | '"' => i = quote_span_end(text, i, end),
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    end.saturating_sub(1)
}

/// The interior of the substitution whose `(` sits at `index`. An unmatched
/// opening keeps the rest of the region visible.
fn parenthesized_span(text: &[char], index: usize, end: usize) -> Span {
    let close = matching_paren(text, index, end);
    if close + 1 == end && at(text, end - 1) != Some(')') {
        return (index + 1, end);
    }
    (index + 1, close)
}

/// The interior of the command substitution starting at `index` (`$(...)`
/// or a backtick pair), or `None` when none starts there.
fn substitution_span(text: &[char], index: usize, end: usize) -> Option<Span> {
    if text[index] == '`' {
        // Only an unescaped backtick ends an old-style substitution.
        let mut close = index + 1;
        while close < end {
            if text[close] == '\\' && close + 1 < end {
                close += 2;
                continue;
            }
            if text[close] == '`' {
                return Some((index + 1, close));
            }
            close += 1;
        }
        return Some((index + 1, end));
    }
    if text[index] == '$' && at(text, index + 1) == Some('(') {
        return Some(parenthesized_span(text, index + 1, end));
    }
    None
}

/// Consume one redirection operator: the index after it, and whether it
/// duplicates a descriptor (`2>&1`, `>&2`), which has no target word.
fn redirect_operator_end(text: &[char], mut index: usize, end: usize) -> (usize, bool) {
    if text[index] == '&' {
        index += 1; // `&>` / `&>>`: both streams, one target word
    }
    while index < end && is_redirect_char(text[index]) {
        index += 1;
    }
    if index < end && text[index] == '&' {
        let mut duplicate_end = index + 1;
        while duplicate_end < end
            && (is_py_digit(text[duplicate_end]) || text[duplicate_end] == '-')
        {
            duplicate_end += 1;
        }
        return (duplicate_end, true);
    }
    (index, false)
}

/// Consume one redirection target word, reporting the command substitutions
/// inside it: the shell takes the redirection out of the argv, but a
/// substitution in the target still runs.
fn skip_redirect_target(text: &[char], mut index: usize, end: usize) -> (usize, Vec<Span>) {
    let mut substitutions = Vec::new();
    let mut quote: Option<char> = None;
    while index < end {
        let char = text[index];
        match quote {
            Some('\'') => {
                if char == '\'' {
                    quote = None;
                }
                index += 1;
                continue;
            }
            Some(_) => {
                if char == '"' {
                    quote = None;
                    index += 1;
                    continue;
                }
                if char == '\\' && index + 1 < end {
                    index += 2;
                    continue;
                }
                if let Some(span) = substitution_span(text, index, end) {
                    substitutions.push(span);
                    index = span.1 + 1;
                    continue;
                }
                index += 1;
                continue;
            }
            None => {}
        }
        if char == '<' && at(text, index + 1) == Some('(') {
            // `sh < <(curl ...)`: the target is a process substitution.
            let span = parenthesized_span(text, index + 1, end);
            substitutions.push(span);
            index = span.1 + 1;
            continue;
        }
        if matches!(char, ' ' | '\t' | '\r' | '\n') || is_separator(char) || is_redirect_char(char)
        {
            break;
        }
        if char == '\\' && index + 1 < end {
            index += 2;
            continue;
        }
        if matches!(char, '\'' | '"') {
            quote = Some(char);
            index += 1;
            continue;
        }
        if let Some(span) = substitution_span(text, index, end) {
            substitutions.push(span);
            index = span.1 + 1;
            continue;
        }
        index += 1;
    }
    (index, substitutions)
}

/// A pending here-document: its delimiter, whether the delimiter was
/// quoted, and the index of the stage that owns the `<<`.
struct PendingHeredoc {
    delimiter: Vec<char>,
    quoted: bool,
    stage: usize,
}

/// Consume a here-document's `<<` operator and its delimiter word: the index
/// after the delimiter, plus the delimiter and whether it was quoted. The
/// body is read at the line's newline, so the line's own pipeline
/// (`cat <<EOF | sh`) stays in the stream to be read as stages.
fn heredoc_delimiter(
    text: &[char],
    mut index: usize,
    end: usize,
) -> (usize, Option<(Vec<char>, bool)>) {
    index += 2;
    if index < end && text[index] == '-' {
        index += 1;
    }
    while index < end && matches!(text[index], ' ' | '\t') {
        index += 1;
    }
    let mut delimiter = Vec::new();
    let mut quoted = false;
    while index < end {
        let char = text[index];
        if matches!(char, '\'' | '"') {
            quoted = true; // quoting a delimiter only suppresses expansion
            index += 1;
            continue;
        }
        if matches!(char, ' ' | '\t' | '\r' | '\n') || is_separator(char) {
            break;
        }
        delimiter.push(char);
        index += 1;
    }
    if delimiter.is_empty() {
        return (index, None);
    }
    (index, Some((delimiter, quoted)))
}

/// Index of the first `\n` at or after `from`, when it lies before `end`.
fn newline_before(text: &[char], from: usize, end: usize) -> Option<usize> {
    let found = text.get(from..)?.iter().position(|char| *char == '\n')? + from;
    (found < end).then_some(found)
}

/// Consume a here-document body at `index` (its opening newline): the index
/// its delimiter line starts at, plus the body's span. A body whose delimiter
/// line never comes runs to the end, and an empty one reads none.
fn heredoc_body(
    text: &[char],
    index: usize,
    end: usize,
    delimiter: &[char],
) -> (usize, Option<Span>) {
    let body_start = index + 1;
    let mut line_start = body_start;
    let mut body_end = end;
    while line_start < end {
        let Some(line_end) = newline_before(text, line_start, end) else {
            break;
        };
        let mut line = &text[line_start..line_end];
        while let ['\t', rest @ ..] = line {
            line = rest;
        }
        while let [rest @ .., '\r'] = line {
            line = rest;
        }
        if line == delimiter {
            body_end = line_start;
            break;
        }
        line_start = line_end + 1;
    }
    if body_end <= body_start {
        return (body_end, None);
    }
    (body_end, Some((body_start, body_end)))
}

/// Python's `str.isdigit` for the characters a shell word can hold.
pub(super) fn is_py_digit(char: char) -> bool {
    char.is_ascii_digit() || (!char.is_ascii() && char.is_numeric())
}

/// The scanner's state while it builds one word and one stage.
struct Scanner<'a> {
    text: &'a [char],
    stages: Vec<Stage>,
    substitutions: Vec<Span>,
    target_substitutions: Vec<Span>,
    words: Vec<Word>,
    chars: String,
    word_started: bool,
    word_substitutions: Vec<Span>,
    word_quoted: bool,
    resolvable: bool,
}

impl Scanner<'_> {
    /// End the current word.
    fn flush_word(&mut self) {
        self.end_word(false);
    }

    /// End the word before a redirection: its descriptor digits (`2>`) are
    /// not an argv word.
    fn flush_before_redirect(&mut self) {
        self.end_word(true);
    }

    fn end_word(&mut self, drop_descriptor: bool) {
        if self.word_started {
            let value = std::mem::take(&mut self.chars);
            let numeric = !value.is_empty() && value.chars().all(is_py_digit);
            if !(drop_descriptor && numeric) {
                self.words.push(Word {
                    value,
                    substitutions: std::mem::take(&mut self.word_substitutions),
                    resolvable: self.resolvable,
                    quoted: self.word_quoted,
                });
            }
        }
        self.chars.clear();
        self.word_started = false;
        self.word_substitutions.clear();
        self.word_quoted = false;
        self.resolvable = true;
    }

    fn end_stage(&mut self, separator: Separator) {
        self.flush_word();
        self.stages.push(Stage {
            separator,
            words: std::mem::take(&mut self.words),
            target_substitutions: std::mem::take(&mut self.target_substitutions),
            heredoc_bodies: Vec::new(),
        });
    }

    /// Keep a substitution in the current word: its text stays in the value
    /// (so the word never resolves) and its interior is reported.
    fn substitution_word(&mut self, index: usize, span: Span) -> usize {
        self.word_started = true;
        self.word_substitutions.push(span);
        self.substitutions.push(span);
        self.resolvable = false;
        self.chars.extend(slice(self.text, index, span.1 + 1));
        span.1 + 1
    }
}

/// Split `text[start..end]` into pipeline stages and their words.
#[expect(
    clippy::too_many_lines,
    reason = "one character-level state machine; splitting it would scatter the quote state"
)]
pub(super) fn scan_region(text: &[char], start: usize, end: usize) -> Region {
    let mut scan = Scanner {
        text,
        stages: Vec::new(),
        substitutions: Vec::new(),
        target_substitutions: Vec::new(),
        words: Vec::new(),
        chars: String::new(),
        word_started: false,
        word_substitutions: Vec::new(),
        word_quoted: false,
        resolvable: true,
    };
    let mut pending_heredocs: Vec<PendingHeredoc> = Vec::new();
    let mut stage_bodies: Vec<(usize, HeredocBody)> = Vec::new();
    let mut quote: Option<char> = None;
    let mut index = start;
    while index < end {
        let char = text[index];
        match quote {
            Some('\'') => {
                if char == '\'' {
                    quote = None;
                } else {
                    scan.chars.push(char);
                }
                index += 1;
                continue;
            }
            Some(_) => {
                if char == '"' {
                    quote = None;
                    index += 1;
                    continue;
                }
                if char == '\\'
                    && index + 1 < end
                    && matches!(text[index + 1], '"' | '\\' | '$' | '`')
                {
                    scan.chars.push(text[index + 1]);
                    index += 2;
                    continue;
                }
                if let Some(span) = substitution_span(text, index, end) {
                    index = scan.substitution_word(index, span);
                    continue;
                }
                if matches!(char, '$' | '`') {
                    scan.resolvable = false; // an expansion the scan cannot follow
                }
                scan.chars.push(char);
                index += 1;
                continue;
            }
            None => {}
        }
        if char == '\\' {
            if at(text, index + 1) == Some('\n') && index + 1 < end {
                index += 2; // line continuation: the word around it continues
                continue;
            }
            if index + 1 < end {
                scan.word_started = true;
                scan.chars.push(text[index + 1]);
                index += 2;
                continue;
            }
            scan.resolvable = false; // a region cut in half by a continuation
            index += 1;
            continue;
        }
        if matches!(char, ' ' | '\t' | '\r') {
            scan.flush_word();
            index += 1;
            continue;
        }
        if char == '#' && !scan.word_started {
            while index < end && text[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if char == '<' && at(text, index + 1) == Some('(') {
            // `<(...)` with no space is a process substitution (a file the
            // command reads); `< (` with a space is a redirect.
            let interior = parenthesized_span(text, index + 1, end);
            index = scan.substitution_word(index, interior);
            continue;
        }
        if is_redirect_char(char) || (char == '&' && matches!(at(text, index + 1), Some('<' | '>')))
        {
            if char == '>' && at(text, index + 1) == Some('(') {
                // A write process substitution: the redirection feeds this
                // process its bytes on stdin, so a runner there executes them.
                scan.flush_before_redirect();
                let interior = parenthesized_span(text, index + 1, end);
                scan.substitutions.push(interior);
                scan.target_substitutions.push(interior);
                index = interior.1 + 1;
                continue;
            }
            if char == '<' && at(text, index + 1) == Some('<') && at(text, index + 2) != Some('<') {
                // A here-document's body is stdin below the line's newline, so
                // only its delimiter is consumed here; the body is attached to
                // the stage that owns the `<<`.
                scan.flush_before_redirect();
                let (next, pending) = heredoc_delimiter(text, index, end);
                index = next;
                if let Some((delimiter, quoted)) = pending {
                    pending_heredocs.push(PendingHeredoc {
                        delimiter,
                        quoted,
                        stage: scan.stages.len(),
                    });
                }
                continue;
            }
            scan.flush_before_redirect();
            let (next, duplicates) = redirect_operator_end(text, index, end);
            index = next;
            if !duplicates {
                while index < end && matches!(text[index], ' ' | '\t') {
                    index += 1;
                }
                if index < end && !is_separator(text[index]) && !is_redirect_char(text[index]) {
                    let (next, nested) = skip_redirect_target(text, index, end);
                    index = next;
                    scan.substitutions.extend(nested.iter().copied());
                    scan.target_substitutions.extend(nested);
                }
            }
            continue;
        }
        if is_separator(char) {
            let mut resume = None;
            if char == '\n' && !pending_heredocs.is_empty() {
                // The bodies start below this newline, in the order their
                // `<<` operators appeared; each ends at its own delimiter
                // line, where the stream resumes.
                let mut cursor = index;
                let mut open_newline = index;
                for pending in &pending_heredocs {
                    let (body_end, body) =
                        heredoc_body(text, open_newline, end, &pending.delimiter);
                    cursor = body_end;
                    if let Some(span) = body {
                        stage_bodies.push((
                            pending.stage,
                            HeredocBody {
                                span,
                                quoted: pending.quoted,
                            },
                        ));
                    }
                    let Some(delimiter_newline) = newline_before(text, cursor, end) else {
                        break;
                    };
                    open_newline = delimiter_newline;
                    cursor = delimiter_newline + 1;
                }
                resume = Some(cursor);
                pending_heredocs.clear();
            }
            let separator = match (char, at(text, index + 1)) {
                ('|', Some('|')) => {
                    index += 1;
                    Separator::Or
                }
                ('&', Some('&')) => {
                    index += 1;
                    Separator::And
                }
                ('|', Some('&')) => {
                    index += 1;
                    Separator::PipeBoth
                }
                ('|', _) => Separator::Pipe,
                ('&', _) => Separator::Background,
                (';', _) => Separator::Semicolon,
                ('(', _) => Separator::OpenParen,
                (')', _) => Separator::CloseParen,
                _ => Separator::Newline,
            };
            scan.end_stage(separator);
            index = resume.unwrap_or(index + 1);
            continue;
        }
        if char == '$' && at(text, index + 1) == Some('\'') {
            scan.word_started = true;
            scan.word_quoted = true;
            index += 2;
            let mut closed = false;
            while index < end {
                if text[index] == '\\' && index + 1 < end {
                    scan.resolvable = false; // ANSI-C escapes can spell any byte
                    scan.chars.push(text[index + 1]);
                    index += 2;
                    continue;
                }
                if text[index] == '\'' {
                    closed = true;
                    index += 1;
                    break;
                }
                scan.chars.push(text[index]);
                index += 1;
            }
            if !closed {
                quote = Some('\'');
            }
            continue;
        }
        if let Some(span) = substitution_span(text, index, end) {
            index = scan.substitution_word(index, span);
            continue;
        }
        if matches!(char, '\'' | '"') {
            scan.word_started = true;
            scan.word_quoted = true;
            quote = Some(char);
            index += 1;
            continue;
        }
        if matches!(char, '$' | '`') {
            scan.resolvable = false; // an expansion the scan cannot follow
        }
        scan.word_started = true;
        scan.chars.push(char);
        index += 1;
    }
    scan.end_stage(Separator::End);
    let mut stages = scan.stages;
    for (position, body) in stage_bodies {
        if let Some(stage) = stages.get_mut(position) {
            stage.heredoc_bodies.push(body);
        }
    }
    Region {
        stages,
        substitutions: scan.substitutions,
        unterminated_quote: quote.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    /// `test_matching_paren_is_the_canonical_quote_aware_scan`.
    #[test]
    fn matching_paren_skips_quoted_and_escaped_parens() {
        for command in [
            "$( : ')'; curl URL)",
            "$(echo \\( && echo x)",
            "$((echo hi) && echo y)",
            "$(printf \"%s\" \")\" && echo x)",
        ] {
            let text = chars(command);
            assert_eq!(
                matching_paren(&text, 1, text.len()),
                text.len() - 1,
                "{command}"
            );
        }
        // An unmatched open never matches, so the interior extends to the end.
        assert_eq!(matching_paren(&chars("$(echo hi"), 1, 8), 7);
    }

    #[test]
    fn a_pipeline_splits_into_stages_with_folded_words() {
        let text = chars("cu\"rl\" -s URL | sh");
        let region = scan_region(&text, 0, text.len());
        let words: Vec<Vec<&str>> = region
            .stages
            .iter()
            .map(|stage| stage.words.iter().map(|word| word.value.as_str()).collect())
            .collect();
        assert_eq!(words, vec![vec!["curl", "-s", "URL"], vec!["sh"]]);
        assert_eq!(
            region
                .stages
                .iter()
                .map(|stage| stage.separator)
                .collect::<Vec<_>>(),
            vec![Separator::Pipe, Separator::End]
        );
    }
}
