//! The sudo guard's shell lexer: words, operators and redirects with their
//! positions and quote-folded values, plus the heredoc bodies they own.
//!
//! Positions are character indices (not byte offsets) into the scanned text,
//! so heredoc bodies, word spans and payload slices line up exactly as the
//! scan reads them.

/// What kind of token a [`Word`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A shell word.
    Word,
    /// A control operator (`;`, `&`, `&&`, `|`, `||`, `(`, `)`).
    Operator,
    /// A redirection with its target (`>file`, `2>&1`, `<<EOF`, `<<<text`).
    Redirect,
    /// A standalone `{` or `}` group brace.
    Group,
}

/// One shell word, operator or redirect with its position and folded value.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-word facts the walk tests one at a time"
)]
pub(super) struct Word {
    pub value: String,
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    /// The word sits where the shell reads a command word.
    pub starts_command: bool,
    /// The word carries a `$`/backtick expansion or a process substitution.
    pub has_expansion: bool,
    /// The word is the detached target of the redirect before it.
    pub is_operand: bool,
    /// `NAME=value` (or `NAME+=value`).
    pub is_assignment: bool,
    /// The word lies inside a heredoc body: data, not a command.
    pub is_data: bool,
    /// The heredoc operator (`<<`, `<<-`, `<<<`) of a heredoc redirect.
    pub heredoc: Option<&'static str>,
    pub heredoc_delim: Option<String>,
    pub heredoc_body: Option<String>,
}

impl Word {
    fn new(value: String, start: usize, end: usize, kind: Kind) -> Self {
        Self {
            value,
            start,
            end,
            kind,
            starts_command: false,
            has_expansion: false,
            is_operand: false,
            is_assignment: false,
            is_data: false,
            heredoc: None,
            heredoc_delim: None,
            heredoc_body: None,
        }
    }

    pub(super) fn is_operator(&self) -> bool {
        self.kind == Kind::Operator
    }

    pub(super) fn is_redirect(&self) -> bool {
        self.kind == Kind::Redirect
    }

    /// `-x`-style flags (a lone `-` is an operand).
    pub(super) fn is_flag(&self) -> bool {
        self.value.starts_with('-') && self.value != "-"
    }
}

const BREAK_CHARS: &[char] = &[';', '&', '|', '(', ')', '<', '>'];
const REDIRECT_OPERATORS: [&str; 10] = ["<<<", "<<-", "<<", ">>", "<>", ">&", "<&", ">|", ">", "<"];

/// Python's `str.isspace`: Unicode whitespace plus the ASCII separators
/// `\x1c`-`\x1f`.
pub(super) fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

/// Python's `str.isdigit` for one character.
pub(super) fn is_digit(ch: char) -> bool {
    ch.is_numeric()
}

/// Python's `str.strip()`.
pub(super) fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Drop quote characters from both ends (`value.strip("\"'")`).
pub(super) fn strip_quotes(value: &str) -> &str {
    value.trim_matches(['"', '\''])
}

/// The characters of `text` from `start` (character index) to the end.
pub(super) fn chars_from(text: &str, start: usize) -> String {
    text.chars().skip(start).collect()
}

/// Drop backslash-newline pairs the way the shell does (not inside single
/// quotes); other escaped pairs stay intact for the lexer to fold.
pub(super) fn join_line_continuations(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::with_capacity(command.len());
    let (mut single, mut double) = (false, false);
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        let following = chars.get(index + 1).copied();
        if ch == '\\' && following == Some('\n') {
            if !single {
                index += 2;
                continue;
            }
            out.push(ch);
            index += 1;
            continue;
        }
        if ch == '\\' && !single {
            if let Some(following) = following {
                out.push(ch);
                out.push(following);
                index += 2;
                continue;
            }
        }
        if ch == '\'' && !double {
            single = !single;
        } else if ch == '"' && !single {
            double = !double;
        }
        out.push(ch);
        index += 1;
    }
    out
}

/// Index just past the quoted span starting at `chars[start]` (a single or
/// double quote), skipping escaped characters inside double quotes; `end`
/// when the span is unterminated.
fn quote_span_end(chars: &[char], start: usize, end: usize) -> usize {
    let quote = chars[start];
    let mut i = start + 1;
    while i < end {
        let ch = chars[i];
        if quote == '"' && ch == '\\' {
            i += 2;
            continue;
        }
        if ch == quote {
            return i + 1;
        }
        i += 1;
    }
    end
}

/// Index of the `)` matching the `(` at `open_index`, or `end - 1`.
///
/// Quote-aware: a `)` inside a single- or double-quoted span or after a
/// backslash never closes the substitution. Unterminated quotes or an
/// unmatched `(` scan to the end, so the whole region stays live command text.
pub(super) fn matching_paren(chars: &[char], open_index: usize, end: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open_index;
    while i < end {
        match chars[i] {
            '\\' => i += 2,
            '\'' | '"' => i = quote_span_end(chars, i, end),
            '(' => {
                depth += 1;
                i += 1;
            }
            ')' => {
                depth = depth.wrapping_sub(1);
                if depth == 0 {
                    return i;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    end.wrapping_sub(1)
}

/// Redirect operator at `start` (optional leading fd digits): the operator
/// and the index after it.
fn match_redirect(chars: &[char], start: usize) -> Option<(&'static str, usize)> {
    let mut index = start;
    while index < chars.len() && is_digit(chars[index]) {
        index += 1;
    }
    REDIRECT_OPERATORS.into_iter().find_map(|operator| {
        let len = operator.chars().count();
        (index + len <= chars.len()
            && chars[index..index + len]
                .iter()
                .copied()
                .eq(operator.chars()))
        .then_some((operator, index + len))
    })
}

/// `NAME=value` or `NAME+=value` with a valid shell name.
pub(super) fn is_assignment(value: &str) -> bool {
    let Some((name, _)) = value.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_alphanumeric() || ch == '_')
}

fn code_point_char(code: u32) -> Option<char> {
    char::from_u32(code)
}

fn ansi_c_simple(code: char) -> Option<char> {
    Some(match code {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' | 'E' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        '\\' => '\\',
        '\'' => '\'',
        '"' => '"',
        '?' => '?',
        _ => return None,
    })
}

/// Decode `$'...'` text from its opening quote: the text and the index after
/// the closing quote (or the end, unterminated). Unknown or out-of-range
/// escapes keep their backslash.
fn read_ansi_c(chars: &[char], quote_index: usize) -> (String, usize) {
    let mut out = String::new();
    let mut index = quote_index + 1;
    let length = chars.len();
    while index < length {
        let ch = chars[index];
        if ch == '\'' {
            return (out, index + 1);
        }
        if ch != '\\' || index + 1 >= length {
            out.push(ch);
            index += 1;
            continue;
        }
        let code = chars[index + 1];
        if let Some(simple) = ansi_c_simple(code) {
            out.push(simple);
            index += 2;
            continue;
        }
        if ('0'..='7').contains(&code) {
            let mut cursor = index + 1;
            let mut value = 0u32;
            let mut digits = 0;
            while cursor < length && digits < 3 && ('0'..='7').contains(&chars[cursor]) {
                value = value * 8 + chars[cursor].to_digit(8).unwrap_or(0);
                digits += 1;
                cursor += 1;
            }
            if let Some(decoded) = code_point_char(value & 0xFF) {
                out.push(decoded);
                index = cursor;
                continue;
            }
        }
        if code == 'x' {
            let mut cursor = index + 2;
            let mut value = 0u32;
            let mut digits = 0;
            while cursor < length && digits < 2 && chars[cursor].is_ascii_hexdigit() {
                value = value * 16 + chars[cursor].to_digit(16).unwrap_or(0);
                digits += 1;
                cursor += 1;
            }
            if digits > 0 {
                if let Some(decoded) = code_point_char(value) {
                    out.push(decoded);
                    index = cursor;
                    continue;
                }
            }
        }
        if code == 'u' || code == 'U' {
            let width = if code == 'u' { 4 } else { 8 };
            let digits = &chars[(index + 2).min(length)..(index + 2 + width).min(length)];
            if digits.len() == width && digits.iter().all(char::is_ascii_hexdigit) {
                let value = digits.iter().fold(0u64, |acc, digit| {
                    acc * 16 + u64::from(digit.to_digit(16).unwrap_or(0))
                });
                if let Some(decoded) = u32::try_from(value).ok().and_then(code_point_char) {
                    out.push(decoded);
                    index += 2 + width;
                    continue;
                }
            }
        }
        if code == 'c' {
            if let Some(&control) = chars.get(index + 2) {
                let mut upper = control.to_uppercase();
                let decoded = match (upper.next(), upper.next()) {
                    (Some(single), None) => code_point_char(u32::from(single) ^ 0x40),
                    _ => None,
                };
                if let Some(decoded) = decoded.filter(|_| control != '\\') {
                    out.push(decoded);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(ch);
        index += 1;
    }
    (out, index)
}

/// Split shell text into words, operators and redirects, folding quotes.
pub(super) fn tokenize(command: &str) -> Vec<Word> {
    Lexer::new(command).run()
}

/// The lexer's state while it walks one text.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the pending word's independent flags, set and cleared at word boundaries"
)]
struct Lexer {
    chars: Vec<char>,
    words: Vec<Word>,
    /// The folded value of the word being built.
    buffer: String,
    /// The open quote (`'` or `"`) the lexer is inside.
    quote: Option<char>,
    started: bool,
    expansion: bool,
    segment_start: bool,
    operand_next: bool,
    word_start: usize,
    index: usize,
}

impl Lexer {
    fn new(command: &str) -> Self {
        Self {
            chars: command.chars().collect(),
            words: Vec::new(),
            buffer: String::new(),
            quote: None,
            started: false,
            expansion: false,
            segment_start: true,
            operand_next: false,
            word_start: 0,
            index: 0,
        }
    }

    fn flush(&mut self) {
        if !self.started {
            return;
        }
        let mut word = Word::new(
            std::mem::take(&mut self.buffer),
            self.word_start,
            self.index,
            Kind::Word,
        );
        word.starts_command = self.segment_start;
        word.has_expansion = self.expansion;
        word.is_operand = self.operand_next;
        self.words.push(word);
        self.started = false;
        self.expansion = false;
        self.segment_start = false;
        self.operand_next = false;
        self.word_start = self.index;
    }

    fn note_character(&mut self) {
        if !self.started {
            self.started = true;
            self.word_start = self.index;
        }
    }

    /// Consume `ch` when it is quoted text, an escape, or a quote opener;
    /// false when the character is unquoted shell syntax or word text.
    fn quoting(&mut self, ch: char) -> bool {
        match self.quote {
            Some('\'') => {
                if ch == '\'' {
                    self.quote = None;
                } else {
                    self.note_character();
                    self.buffer.push(ch);
                }
                self.index += 1;
                return true;
            }
            Some(_) => {
                if ch == '"' {
                    self.quote = None;
                } else {
                    self.note_character();
                    if ch == '$' || ch == '`' {
                        self.expansion = true;
                    }
                    self.buffer.push(ch);
                }
                self.index += 1;
                return true;
            }
            None => {}
        }
        let next = self.chars.get(self.index + 1).copied();
        match (ch, next) {
            ('\\', Some(escaped)) => {
                self.note_character();
                self.buffer.push(escaped);
                self.index += 2;
            }
            ('\'' | '"', _) => {
                self.note_character();
                self.quote = Some(ch);
                self.index += 1;
            }
            ('$', Some('\'')) => {
                // $'...' is ANSI-C text: decode it so escapes cannot hide a name.
                self.note_character();
                let (text, after) = read_ansi_c(&self.chars, self.index + 1);
                if text.contains(['$', '`']) {
                    self.expansion = true;
                }
                self.buffer.push_str(&text);
                self.index = after;
            }
            ('$', Some('"')) => {
                // $"..." folds like double quotes.
                self.note_character();
                self.quote = Some('"');
                self.index += 2;
            }
            _ => return false,
        }
        true
    }

    fn run(mut self) -> Vec<Word> {
        let length = self.chars.len();
        while self.index < length {
            let ch = self.chars[self.index];
            if self.quoting(ch) {
                continue;
            }
            if ch == '#' && !self.started {
                while self.index < length && self.chars[self.index] != '\n' {
                    self.index += 1;
                }
                continue;
            }
            if matches!(ch, '\t' | '\n' | ' ' | '\r' | '\u{b}' | '\u{c}') {
                self.flush();
                if ch == '\n' {
                    self.segment_start = true;
                }
                self.index += 1;
                continue;
            }
            if (ch == '<' || ch == '>') && self.chars.get(self.index + 1) == Some(&'(') {
                // A process substitution runs its span as a command of its
                // own: keep it in one word so the span scan recurses into it.
                self.note_character();
                let close = matching_paren(&self.chars, self.index + 1, length);
                self.expansion = true;
                self.buffer
                    .extend(&self.chars[self.index..(close + 1).min(length)]);
                self.index = close + 1;
                continue;
            }
            let redirect = if self.started || is_digit(ch) || ch == '<' || ch == '>' {
                match_redirect(&self.chars, self.index)
            } else {
                None
            };
            if let Some((operator, after)) = redirect {
                self.lex_redirect(operator, after);
                continue;
            }
            if BREAK_CHARS.contains(&ch) {
                self.flush();
                let doubled =
                    (ch == '&' || ch == '|') && self.chars.get(self.index + 1) == Some(&ch);
                let len = if doubled { 2 } else { 1 };
                let value: String = self.chars[self.index..self.index + len].iter().collect();
                let operator = Word::new(value, self.index, self.index + len, Kind::Operator);
                self.words.push(operator);
                self.segment_start = true;
                self.index += len;
                continue;
            }
            self.note_character();
            if ch == '$' || ch == '`' {
                self.expansion = true;
            }
            self.buffer.push(ch);
            self.index += 1;
        }
        self.flush();
        classify(&mut self.words);
        self.words
    }

    /// A redirect and its glued target (a quoted target is one word:
    /// `bash<<<"sh -c 'sudo id'"` is a script).
    fn lex_redirect(&mut self, operator: &'static str, after: usize) {
        self.flush();
        let length = self.chars.len();
        let mut target_end = after;
        let mut quote: Option<char> = None;
        while target_end < length {
            let ch = self.chars[target_end];
            match quote {
                None => {
                    if is_space(ch) || BREAK_CHARS.contains(&ch) {
                        break;
                    }
                    if ch == '\'' || ch == '"' {
                        quote = Some(ch);
                    }
                }
                Some(open) if ch == open => quote = None,
                Some(_) => {}
            }
            target_end += 1;
        }
        let target: String = self.chars[after..target_end].iter().collect();
        let heredoc = operator.starts_with("<<");
        let mut value: String = self.chars[self.index..after].iter().collect();
        value.push_str(&target);
        let mut word = Word::new(value, self.index, target_end, Kind::Redirect);
        word.starts_command = self.segment_start;
        word.is_operand = self.operand_next;
        word.heredoc = heredoc.then_some(operator);
        word.heredoc_delim =
            (heredoc && !target.is_empty()).then(|| strip_quotes(&target).to_string());
        self.words.push(word);
        self.segment_start = false;
        self.operand_next = target.is_empty();
        self.index = target_end;
    }
}

/// Mark assignments and standalone group braces; `}` starts the next command.
fn classify(words: &mut [Word]) {
    for index in 0..words.len() {
        if words[index].kind != Kind::Word {
            continue;
        }
        if words[index].value == "{" || words[index].value == "}" {
            words[index].kind = Kind::Group;
            if let Some(next) = words.get_mut(index + 1) {
                next.starts_command = true;
            }
        } else if is_assignment(&words[index].value) {
            words[index].is_assignment = true;
        }
    }
}

/// Resolve heredoc delimiters and mark body words as data (not commands).
pub(super) fn apply_heredocs(text: &str, words: &mut [Word]) {
    for index in 0..words.len() {
        let word = &words[index];
        let has_delim = word
            .heredoc_delim
            .as_deref()
            .is_some_and(|delim| !delim.is_empty());
        if word.heredoc.is_none_or(|op| op == "<<<") || has_delim {
            continue;
        }
        if let Some(next) = words.get(index + 1) {
            let delim = strip_quotes(&next.value).to_string();
            words[index].heredoc_delim = Some(delim);
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let find_newline = |from: usize| (from..chars.len()).find(|&i| chars[i] == '\n');
    for index in 0..words.len() {
        let word = &words[index];
        let Some(delim) = word.heredoc_delim.clone().filter(|delim| !delim.is_empty()) else {
            continue;
        };
        if word.heredoc.is_none_or(|op| op == "<<<") {
            continue;
        }
        let Some(newline) = find_newline(word.end) else {
            continue;
        };
        let body_start = newline + 1;
        let mut cursor = body_start;
        let body_end = loop {
            if cursor > chars.len() {
                break chars.len();
            }
            let line_end = find_newline(cursor).unwrap_or(chars.len());
            let line: String = chars[cursor..line_end].iter().collect();
            if strip(&line) == delim {
                break cursor;
            }
            cursor = line_end + 1;
        };
        words[index].heredoc_body = Some(chars[body_start..body_end].iter().collect());
        for other in &mut *words {
            // Only the body itself is data; the rest of the line still runs.
            if (body_start..body_end).contains(&other.start) {
                other.is_data = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_paren_keeps_the_three_arg_family_contract() {
        let text: Vec<char> = "$(sudo id)".chars().collect();
        assert_eq!(matching_paren(&text, 1, 10), 9);
        let text: Vec<char> = "$(sudo id".chars().collect();
        assert_eq!(matching_paren(&text, 1, 9), 8);
    }

    #[test]
    fn redirect_targets_and_heredoc_bodies() {
        let text = "cat <<EOF | sh\nsudo id\nEOF";
        let mut words = tokenize(text);
        apply_heredocs(text, &mut words);
        let summary: Vec<(&str, Kind, bool, bool)> = words
            .iter()
            .map(|w| (w.value.as_str(), w.kind, w.starts_command, w.is_data))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("cat", Kind::Word, true, false),
                ("<<EOF", Kind::Redirect, false, false),
                ("|", Kind::Operator, false, false),
                ("sh", Kind::Word, true, false),
                ("sudo", Kind::Word, true, true),
                ("id", Kind::Word, false, true),
                ("EOF", Kind::Word, true, false),
            ]
        );
        assert_eq!(words[1].heredoc_body.as_deref(), Some("sudo id\n"));
    }

    #[test]
    fn ansi_c_folding_decodes_escapes() {
        let words = tokenize("$'su\\x64o' $'su\\144o' $'\\U00110000'");
        let values: Vec<&str> = words.iter().map(|w| w.value.as_str()).collect();
        assert_eq!(values, vec!["sudo", "sudo", "\\U00110000"]);
    }
}
