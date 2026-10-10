//! The hand-written bash parser: one pass over the text, quoting kept per
//! word part, here-document bodies read at the newline that ends their
//! line, and every nested context (`$(...)`, backquotes, `<(...)`,
//! compound commands) parsed by the same recursive descent.
//!
//! The parser never rejects input. What bash would refuse as a syntax error
//! still yields the commands read so far (bash runs the lines before a
//! syntax error); text past nesting [`MAX_DEPTH`] is kept unread as
//! [`Command::Unparsed`]. Every byte is consumed once, so the cost is linear
//! in the text (a here-document opener costs one line scan for its body).

use super::ast::{
    AndOr, AssignValue, Assignment, Command, Connector, HereDoc, List, Part, Pipeline, Quoting,
    Redirect, RedirectOp, SimpleCommand, Word,
};

/// Nesting bound for substitutions and compound commands. Deeper text is
/// kept as [`Command::Unparsed`].
pub(crate) const MAX_DEPTH: usize = 64;

#[cfg(test)]
thread_local! {
    /// Parser steps taken on this thread (tests bound cost by counting
    /// work, not by timing it).
    pub(crate) static WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_work(steps: usize) {
    WORK.with(|work| work.set(work.get() + steps));
}

#[cfg(not(test))]
fn count_work(_steps: usize) {}

/// The descriptor a `{name}>file` redirect allocates (never 0, 1 or 2).
pub(crate) const NEW_DESCRIPTOR: u32 = u32::MAX;

/// A parse result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Parsed {
    pub list: List,
    /// The text read as bash reads it: no unbalanced quote or bracket, no
    /// stray keyword, nothing left unread.
    pub complete: bool,
    /// The text bash refuses to run: the line holding the first syntax
    /// error and everything after it (the commands before it run).
    pub dropped: Option<String>,
}

/// Parse `text` as a bash script.
pub(crate) fn parse(text: &str) -> Parsed {
    parse_at_depth(text, 0)
}

pub(crate) fn parse_at_depth(text: &str, depth: usize) -> Parsed {
    let mut parser = Parser::new(text, depth);
    let mut list = parser.list(Stop::End);
    parser.fill_heredocs(&mut list);
    Parsed {
        list,
        complete: !parser.broken,
        dropped: parser.dropped.filter(|text| !text.trim().is_empty()),
    }
}

/// Where a list ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The end of the text.
    End,
    /// A `)` (subshell, substitution).
    Paren,
    /// A reserved word in command position (`then`, `fi`, `done`, `}`...),
    /// or a case-arm terminator.
    Keyword,
}

const CLOSERS: [&str; 8] = ["then", "elif", "else", "fi", "do", "done", "esac", "}"];

struct PendingHeredoc {
    index: usize,
    delimiter: String,
    strip_tabs: bool,
    quoted: bool,
}

struct Parser<'s> {
    src: &'s str,
    b: &'s [u8],
    pos: usize,
    depth: usize,
    pending: Vec<PendingHeredoc>,
    bodies: Vec<Option<HereDoc>>,
    broken: bool,
    /// Nesting past the bound ended the read early (closers then go
    /// missing; that is not a syntax error).
    truncated: bool,
    /// How many `$(...)` / `<(...)` bodies enclose the cursor.
    substitutions: usize,
    /// The text bash would not run (from the line of the first syntax
    /// error on).
    dropped: Option<String>,
}

fn is_blank(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn is_meta(byte: u8) -> bool {
    matches!(byte, b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')')
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

impl<'s> Parser<'s> {
    fn new(src: &'s str, depth: usize) -> Self {
        Self {
            src,
            b: src.as_bytes(),
            pos: 0,
            depth,
            pending: Vec::new(),
            bodies: Vec::new(),
            broken: false,
            truncated: false,
            substitutions: 0,
            dropped: None,
        }
    }

    fn peek(&self) -> Option<u8> {
        count_work(1);
        self.b.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        count_work(1);
        self.b.get(self.pos + offset).copied()
    }

    fn at(&self, text: &str) -> bool {
        count_work(text.len());
        self.b[self.pos..].starts_with(text.as_bytes())
    }

    fn eof(&self) -> bool {
        self.pos >= self.b.len()
    }

    /// The char starting at the cursor (the cursor is always on a char
    /// boundary: it only stops after ASCII bytes or whole chars).
    fn take_char(&mut self) -> char {
        match self.src[self.pos..].chars().next() {
            Some(ch) => {
                self.pos += ch.len_utf8();
                ch
            }
            None => '\0',
        }
    }

    /// The rest of the text, unread: nesting went past the bound. (Bash
    /// runs it; only the parser stops, so this is not a syntax error.)
    fn unparsed_rest(&mut self) -> Command {
        let rest = self.src[self.pos..].to_string();
        self.pos = self.b.len();
        self.truncated = true;
        Command::Unparsed(rest)
    }

    // ---- blanks, comments, newlines -------------------------------------

    fn skip_blanks(&mut self) {
        loop {
            match self.peek() {
                Some(byte) if is_blank(byte) => self.pos += 1,
                Some(b'\\') if self.peek_at(1) == Some(b'\n') => self.pos += 2,
                Some(b'\\') if self.peek_at(1) == Some(b'\r') && self.peek_at(2) == Some(b'\n') => {
                    self.pos += 3;
                }
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => self.pos += 1,
                _ => return,
            }
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some(b'#') {
            while let Some(byte) = self.peek() {
                if byte == b'\n' {
                    break;
                }
                self.pos += 1;
            }
        }
    }

    /// Blanks and a comment on this line.
    fn skip_space(&mut self) {
        self.skip_blanks();
        self.skip_comment();
    }

    /// Consume a newline and read the here-document bodies it releases.
    fn newline(&mut self) {
        self.pos += 1;
        if !self.pending.is_empty() {
            self.read_heredoc_bodies();
        }
    }

    fn skip_linebreaks(&mut self) {
        loop {
            self.skip_space();
            if self.peek() == Some(b'\n') {
                self.newline();
            } else {
                return;
            }
        }
    }

    fn read_heredoc_bodies(&mut self) {
        for pending in std::mem::take(&mut self.pending) {
            let start = self.pos;
            let mut end = self.b.len();
            let mut resume = self.b.len();
            let mut line_start = self.pos;
            while line_start < self.b.len() {
                let line_end = memchr::memchr(b'\n', &self.b[line_start..])
                    .map_or(self.b.len(), |at| line_start + at);
                count_work(line_end - line_start + 1);
                let mut line = &self.src[line_start..line_end];
                line = line.strip_suffix('\r').unwrap_or(line);
                if pending.strip_tabs {
                    line = line.trim_start_matches('\t');
                }
                if line == pending.delimiter {
                    end = line_start;
                    resume = (line_end + 1).min(self.b.len());
                    break;
                }
                // Inside `$(...)` bash ends the body with the substitution:
                // `EOF)` closes both.
                if self.substitutions > 0
                    && line.starts_with(pending.delimiter.as_str())
                    && line[pending.delimiter.len()..]
                        .trim_start()
                        .starts_with(')')
                {
                    end = line_start;
                    let raw = &self.src[line_start..line_end];
                    let lead = if pending.strip_tabs {
                        raw.len() - raw.trim_start_matches('\t').len()
                    } else {
                        0
                    };
                    resume = line_start + lead + pending.delimiter.len();
                    break;
                }
                line_start = line_end + 1;
            }
            let mut raw = self.src[start..end].to_string();
            if pending.strip_tabs {
                raw = raw
                    .split_inclusive('\n')
                    .map(|line| line.trim_start_matches('\t'))
                    .collect();
            }
            self.pos = resume;
            let body = if pending.quoted {
                Word {
                    parts: vec![Part::Literal(raw.clone(), Quoting::Single)],
                    raw: raw.clone(),
                }
            } else {
                expand_text(&raw, self.depth + 1)
            };
            if let Some(slot) = self.bodies.get_mut(pending.index) {
                *slot = Some(HereDoc {
                    raw,
                    body,
                    slot: None,
                });
            }
        }
    }

    fn fill_heredocs(&mut self, list: &mut List) {
        if self.bodies.is_empty() {
            return;
        }
        let mut bodies = std::mem::take(&mut self.bodies);
        super::walk::visit_redirects_mut(list, &mut |redirect| {
            if let Some(heredoc) = redirect.heredoc.as_mut() {
                if let Some(index) = heredoc.slot.take() {
                    if let Some(body) = bodies.get_mut(index).and_then(Option::take) {
                        *heredoc = body;
                    }
                }
            }
        });
    }

    // ---- lists ----------------------------------------------------------

    /// Whether the cursor sits on the bare word `word` followed by a
    /// delimiter.
    fn at_keyword(&self, word: &str) -> bool {
        self.at(word)
            && self
                .b
                .get(self.pos + word.len())
                .is_none_or(|&byte| is_blank(byte) || byte == b'\n' || is_meta(byte))
    }

    fn at_closer(&self) -> bool {
        CLOSERS.iter().any(|word| self.at_keyword(word))
    }

    fn at_case_terminator(&self) -> bool {
        self.at(";;") || self.at(";&")
    }

    fn list(&mut self, stop: Stop) -> List {
        let mut list = List::default();
        // Bash reads the text a line at a time and runs nothing of a line
        // that holds a syntax error (nor anything after it): at the top
        // level the first error drops its whole line, kept as `dropped`.
        let mut line_items = 0;
        let mut line_pos = self.pos;
        loop {
            let gap = self.pos;
            self.skip_linebreaks();
            if stop == Stop::End && self.src[gap..self.pos].contains('\n') {
                line_items = list.items.len();
                line_pos = self.pos;
            }
            if stop == Stop::End && self.broken && !self.truncated {
                list.items.truncate(line_items);
                self.dropped = Some(self.src[line_pos..].to_string());
                self.pos = self.b.len();
                break;
            }
            if self.eof() {
                break;
            }
            match stop {
                Stop::Paren if self.peek() == Some(b')') => break,
                Stop::Keyword if self.at_closer() || self.at_case_terminator() => break,
                Stop::End | Stop::Paren | Stop::Keyword => {}
            }
            if self.peek() == Some(b')') || self.at_case_terminator() || self.at_closer() {
                // A closer nothing opened: bash's syntax error. Skip it so
                // the rest is still read.
                self.broken = true;
                if self.peek() == Some(b')') || self.at(";&") {
                    self.pos += if self.at(";;&") {
                        3
                    } else {
                        1 + usize::from(self.at(";&"))
                    };
                } else if self.at(";;") {
                    self.pos += 2;
                } else {
                    while self
                        .peek()
                        .is_some_and(|byte| !is_blank(byte) && byte != b'\n' && !is_meta(byte))
                    {
                        self.pos += 1;
                    }
                }
                continue;
            }
            let before = self.pos;
            let Some(mut item) = self.and_or() else {
                if self.pos == before {
                    // An operator with no command before it (`; x`, `| x`).
                    self.broken = true;
                    self.pos += 1;
                }
                continue;
            };
            self.skip_space();
            match self.peek() {
                Some(b'&') if !self.at("&&") && !self.at("&>") => {
                    self.pos += 1;
                    item.background = true;
                }
                Some(b';') if !self.at(";;") && !self.at(";&") => self.pos += 1,
                Some(b'\n') => self.newline(),
                _ => {}
            }
            let ended_line =
                self.b.get(self.pos.wrapping_sub(1)) == Some(&b'\n') && self.pos > before;
            list.items.push(item);
            if self.pos == before {
                self.broken = true;
                self.pos += 1;
            }
            if stop == Stop::End && ended_line && !self.broken {
                line_items = list.items.len();
                line_pos = self.pos;
            }
        }
        if stop == Stop::End && self.broken && !self.truncated && self.dropped.is_none() {
            list.items.truncate(line_items);
            self.dropped = Some(self.src[line_pos.min(self.b.len())..].to_string());
        }
        list
    }

    fn and_or(&mut self) -> Option<AndOr> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            self.skip_space();
            let connector = if self.at("&&") {
                Connector::And
            } else if self.at("||") {
                Connector::Or
            } else {
                break;
            };
            self.pos += 2;
            self.skip_linebreaks();
            if let Some(pipeline) = self.pipeline() {
                rest.push((connector, pipeline));
            } else {
                self.broken = true;
                break;
            }
        }
        Some(AndOr {
            first,
            rest,
            background: false,
        })
    }

    fn pipeline(&mut self) -> Option<Pipeline> {
        self.skip_space();
        let mut negated = false;
        loop {
            if self.at_keyword("!") {
                negated = !negated;
                self.pos += 1;
            } else if self.at_keyword("time") {
                self.pos += 4;
                self.skip_blanks();
                if self.at_keyword("-p") {
                    self.pos += 2;
                    self.skip_blanks();
                }
                if self.at_keyword("--") {
                    self.pos += 2;
                }
            } else {
                break;
            }
            self.skip_space();
        }
        let mut commands = vec![self.command()?];
        loop {
            self.skip_space();
            if self.at("||") || self.peek() != Some(b'|') {
                break;
            }
            self.pos += if self.at("|&") { 2 } else { 1 };
            self.skip_linebreaks();
            if let Some(command) = self.command() {
                commands.push(command);
            } else {
                self.broken = true;
                break;
            }
        }
        Some(Pipeline { negated, commands })
    }

    // ---- commands -------------------------------------------------------

    fn command(&mut self) -> Option<Command> {
        self.skip_space();
        if self.depth > MAX_DEPTH {
            return Some(self.unparsed_rest());
        }
        if self.eof() {
            return None;
        }
        if self.at("((") && self.arithmetic_closes(self.pos + 2) {
            self.pos += 2;
            let expression = self.arithmetic_body();
            self.trailing_redirects();
            return Some(Command::Arithmetic(expression));
        }
        if self.at_keyword("coproc") {
            // `coproc [NAME] COMMAND`: the command runs in the background.
            self.pos += 6;
            self.skip_blanks();
            let start = self.pos;
            while self.peek().is_some_and(is_name) {
                self.pos += 1;
            }
            let named = self.pos > start && {
                let save = self.pos;
                self.skip_blanks();
                let compound = self.peek() == Some(b'(')
                    || ["{", "if", "while", "until", "for", "case", "select", "[["]
                        .iter()
                        .any(|word| self.at_keyword(word));
                if !compound {
                    self.pos = save;
                }
                compound
            };
            if !named {
                self.pos = start;
            }
            return self.command();
        }
        if self.peek() == Some(b'(') {
            self.pos += 1;
            let body = self.nested(Stop::Paren);
            self.expect_byte(b')');
            let redirects = self.trailing_redirects();
            return Some(Command::Subshell(body, redirects));
        }
        if self.at_keyword("{") {
            self.pos += 1;
            let body = self.nested(Stop::Keyword);
            self.expect_keyword("}");
            let redirects = self.trailing_redirects();
            return Some(Command::Group(body, redirects));
        }
        if self.at_keyword("if") {
            self.pos += 2;
            return Some(self.if_command());
        }
        if self.at_keyword("while") || self.at_keyword("until") {
            self.pos += 5;
            let condition = self.nested(Stop::Keyword);
            self.expect_keyword("do");
            let body = self.nested(Stop::Keyword);
            self.expect_keyword("done");
            let redirects = self.trailing_redirects();
            return Some(Command::Branches(vec![condition, body], redirects));
        }
        if self.at_keyword("for") || self.at_keyword("select") {
            self.pos += if self.at("for") { 3 } else { 6 };
            return Some(self.for_command());
        }
        if self.at_keyword("case") {
            self.pos += 4;
            return Some(self.case_command());
        }
        if self.at_keyword("function") {
            self.pos += 8;
            self.skip_blanks();
            let name = self.read_word().map(|word| word.raw).unwrap_or_default();
            self.skip_blanks();
            if self.at("()") {
                self.pos += 2;
            } else if self.peek() == Some(b'(') {
                // `function f ()` or `function f (body)`.
                let save = self.pos;
                self.pos += 1;
                self.skip_blanks();
                if self.peek() == Some(b')') {
                    self.pos += 1;
                } else {
                    self.pos = save;
                }
            }
            return Some(self.function_body(name));
        }
        if self.at_keyword("[[") {
            self.pos += 2;
            return Some(Command::Conditional(self.conditional_words()));
        }
        self.simple_command()
    }

    fn nested(&mut self, stop: Stop) -> List {
        self.depth += 1;
        let list = if self.depth > MAX_DEPTH {
            List {
                items: vec![single(self.unparsed_rest())],
            }
        } else {
            self.list(stop)
        };
        self.depth -= 1;
        list
    }

    fn expect_byte(&mut self, byte: u8) {
        self.skip_linebreaks();
        if self.peek() == Some(byte) {
            self.pos += 1;
        } else {
            self.broken = true;
        }
    }

    fn expect_keyword(&mut self, word: &str) {
        self.skip_linebreaks();
        if self.at_keyword(word) {
            self.pos += word.len();
        } else {
            self.broken = true;
        }
    }

    fn skip_empty_parens(&mut self) {
        self.pos += 1;
        self.skip_blanks();
        if self.peek() == Some(b')') {
            self.pos += 1;
        } else {
            self.broken = true;
        }
    }

    fn trailing_redirects(&mut self) -> Vec<Redirect> {
        let mut redirects = Vec::new();
        loop {
            self.skip_blanks();
            match self.redirect() {
                Some(redirect) => redirects.push(redirect),
                None => return redirects,
            }
        }
    }

    fn if_command(&mut self) -> Command {
        let mut lists = vec![self.nested(Stop::Keyword)];
        self.expect_keyword("then");
        lists.push(self.nested(Stop::Keyword));
        loop {
            self.skip_linebreaks();
            if self.at_keyword("elif") {
                self.pos += 4;
                lists.push(self.nested(Stop::Keyword));
                self.expect_keyword("then");
                lists.push(self.nested(Stop::Keyword));
            } else if self.at_keyword("else") {
                self.pos += 4;
                lists.push(self.nested(Stop::Keyword));
            } else {
                break;
            }
        }
        self.expect_keyword("fi");
        let redirects = self.trailing_redirects();
        Command::Branches(lists, redirects)
    }

    fn for_command(&mut self) -> Command {
        self.skip_blanks();
        if self.at("((") {
            self.pos += 2;
            let header = self.arithmetic_body();
            self.skip_space();
            if self.peek() == Some(b';') {
                self.pos += 1;
            }
            self.skip_linebreaks();
            let body = self.loop_body();
            let redirects = self.trailing_redirects();
            return Command::Branches(
                vec![
                    List {
                        items: vec![single(Command::Arithmetic(header))],
                    },
                    body,
                ],
                redirects,
            );
        }
        let name = self.read_word().map(|word| word.raw).unwrap_or_default();
        self.skip_linebreaks();
        let mut words = None;
        if self.at_keyword("in") {
            self.pos += 2;
            let mut list = Vec::new();
            loop {
                self.skip_blanks();
                match self.peek() {
                    None | Some(b'\n' | b';') => break,
                    Some(_) => match self.read_word() {
                        Some(word) => list.push(word),
                        None => break,
                    },
                }
            }
            words = Some(list);
        }
        self.skip_space();
        if self.peek() == Some(b';') {
            self.pos += 1;
        }
        self.skip_linebreaks();
        let body = self.loop_body();
        let redirects = self.trailing_redirects();
        Command::For {
            name,
            words,
            body,
            redirects,
        }
    }

    /// `do LIST done`, or the legacy `{ LIST; }`.
    fn loop_body(&mut self) -> List {
        if self.at_keyword("{") {
            self.pos += 1;
            let body = self.nested(Stop::Keyword);
            self.expect_keyword("}");
            return body;
        }
        self.expect_keyword("do");
        let body = self.nested(Stop::Keyword);
        self.expect_keyword("done");
        body
    }

    fn case_command(&mut self) -> Command {
        self.skip_blanks();
        let word = self.read_word().unwrap_or_default();
        self.skip_linebreaks();
        self.expect_keyword("in");
        let mut arms = Vec::new();
        loop {
            self.skip_linebreaks();
            if self.eof() {
                self.broken = true;
                break;
            }
            if self.at_keyword("esac") {
                self.pos += 4;
                break;
            }
            if self.peek() == Some(b'(') {
                self.pos += 1;
            }
            let mut patterns = Vec::new();
            loop {
                self.skip_blanks();
                if let Some(pattern) = self.read_word() {
                    patterns.push(pattern);
                }
                self.skip_blanks();
                if self.peek() == Some(b'|') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            if self.peek() == Some(b')') {
                self.pos += 1;
            } else {
                // Not a pattern list: give up on the arms, keep reading.
                self.broken = true;
                if patterns.is_empty() {
                    break;
                }
            }
            let body = self.nested(Stop::Keyword);
            arms.push((patterns, body));
            self.skip_linebreaks();
            if self.at(";;&") {
                self.pos += 3;
            } else if self.at(";;") || self.at(";&") {
                self.pos += 2;
            }
        }
        let redirects = self.trailing_redirects();
        Command::Case {
            word,
            arms,
            redirects,
        }
    }

    fn function_body(&mut self, name: String) -> Command {
        self.skip_linebreaks();
        self.depth += 1;
        let body = self
            .command()
            .unwrap_or(Command::Group(List::default(), Vec::new()));
        self.depth -= 1;
        Command::Function {
            name,
            body: Box::new(body),
        }
    }

    fn conditional_words(&mut self) -> Vec<Word> {
        let mut words = Vec::new();
        loop {
            self.skip_blanks();
            match self.peek() {
                None => {
                    self.broken = true;
                    break;
                }
                Some(b'\n') => self.newline(),
                Some(_) if self.at_keyword("]]") => {
                    self.pos += 2;
                    break;
                }
                Some(b'(' | b')' | b'<' | b'>' | b'!') => {
                    let start = self.pos;
                    self.pos += 1;
                    words.push(Word::literal(&self.src[start..self.pos]));
                }
                Some(b'&' | b'|') => {
                    let start = self.pos;
                    self.pos += if self.at("&&") || self.at("||") { 2 } else { 1 };
                    words.push(Word::literal(&self.src[start..self.pos]));
                }
                Some(_) => {
                    let regex = words.last().is_some_and(|word| word.raw == "=~");
                    let word = if regex {
                        self.regex_operand()
                    } else {
                        self.read_word()
                    };
                    if let Some(word) = word {
                        words.push(word);
                    } else {
                        self.pos += 1;
                        self.broken = true;
                    }
                }
            }
        }
        words
    }

    /// The right operand of `=~`: parentheses and `|` are part of it.
    fn regex_operand(&mut self) -> Option<Word> {
        let start = self.pos;
        let mut parens = 0usize;
        let mut parts = Vec::new();
        let mut text = String::new();
        while let Some(byte) = self.peek() {
            match byte {
                b'(' => parens += 1,
                b')' if parens > 0 => parens -= 1,
                b' ' | b'\t' | b'\n' if parens == 0 => break,
                b')' => break,
                b'\'' | b'"' | b'$' | b'`' | b'\\' => {
                    if !text.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut text), Quoting::Bare));
                    }
                    if let Some(word) = self.read_word() {
                        parts.extend(word.parts);
                    }
                    continue;
                }
                _ => {}
            }
            text.push(self.take_char());
        }
        if !text.is_empty() {
            parts.push(Part::Literal(text, Quoting::Bare));
        }
        (self.pos > start).then(|| Word {
            parts,
            raw: self.src[start..self.pos].to_string(),
        })
    }

    fn simple_command(&mut self) -> Option<Command> {
        let mut command = SimpleCommand::default();
        loop {
            self.skip_blanks();
            match self.peek() {
                None | Some(b'\n' | b';' | b'|' | b')') => break,
                Some(b'&') if !self.at("&>") => break,
                Some(b'#') => {
                    self.skip_comment();
                    break;
                }
                Some(b'(') => {
                    if command.words.len() == 1
                        && command.assignments.is_empty()
                        && command.redirects.is_empty()
                    {
                        // `name ()`: a function definition.
                        let name = command.words.remove(0).raw;
                        self.skip_empty_parens();
                        return Some(self.function_body(name));
                    }
                    self.broken = true;
                    break;
                }
                Some(_) => {}
            }
            if let Some(redirect) = self.redirect() {
                command.redirects.push(redirect);
                continue;
            }
            if command.words.is_empty() {
                if let Some(assignment) = self.assignment() {
                    command.assignments.push(assignment);
                    continue;
                }
            } else if command
                .words
                .first()
                .and_then(Word::as_static)
                .is_some_and(|word| {
                    matches!(
                        word.as_str(),
                        "declare" | "typeset" | "local" | "export" | "readonly"
                    )
                })
            {
                // `declare -A m=( [k]=v )`: a compound assignment operand.
                let start = self.pos;
                if let Some(Assignment {
                    value: AssignValue::Array(values),
                    ..
                }) = self.assignment()
                {
                    let raw = self.src[start..self.pos].to_string();
                    let mut parts = vec![Part::Literal(raw.clone(), Quoting::Single)];
                    for value in values {
                        parts.extend(value.parts.into_iter().filter(Part::is_expansion));
                    }
                    command.words.push(Word { parts, raw });
                    continue;
                }
                self.pos = start;
            }
            if let Some(word) = self.read_word() {
                command.words.push(word);
            } else {
                if self.peek().is_some_and(|byte| !is_meta(byte)) {
                    self.pos += 1;
                }
                self.broken = true;
                break;
            }
        }
        let empty = command.words.is_empty()
            && command.assignments.is_empty()
            && command.redirects.is_empty();
        (!empty).then_some(Command::Simple(command))
    }

    fn assignment(&mut self) -> Option<Assignment> {
        let start = self.pos;
        let b = self.b;
        if !b.get(start).copied().is_some_and(is_name_start) {
            return None;
        }
        let mut at = start + 1;
        while b.get(at).copied().is_some_and(is_name) {
            at += 1;
        }
        let name_end = at;
        if b.get(at) == Some(&b'[') {
            let close = memchr::memchr(b']', &b[at..])?;
            at += close + 1;
        }
        let append = b.get(at) == Some(&b'+');
        if append {
            at += 1;
        }
        if b.get(at) != Some(&b'=') {
            return None;
        }
        let name = self.src[start..name_end].to_string();
        self.pos = at + 1;
        if self.peek() == Some(b'(') {
            self.pos += 1;
            let mut values = Vec::new();
            loop {
                self.skip_linebreaks();
                match self.peek() {
                    None => {
                        self.broken = true;
                        break;
                    }
                    Some(b')') => {
                        self.pos += 1;
                        break;
                    }
                    Some(_) => {
                        if let Some(word) = self.read_word() {
                            values.push(word);
                        } else {
                            self.broken = true;
                            self.pos += 1;
                        }
                    }
                }
            }
            return Some(Assignment {
                name,
                value: AssignValue::Array(values),
                append,
            });
        }
        let value = self.read_word().unwrap_or_default();
        Some(Assignment {
            name,
            value: AssignValue::Scalar(value),
            append,
        })
    }

    fn redirect(&mut self) -> Option<Redirect> {
        let start = self.pos;
        let mut at = start;
        let mut fd = None;
        while self.b.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at > start {
            fd = self.src[start..at].parse().ok();
        } else if self.b.get(at) == Some(&b'{') {
            // `{name}>file`: a descriptor variable.
            let close = self.b[at..].iter().position(|&byte| byte == b'}')?;
            let name = &self.b[at + 1..at + close];
            if name.is_empty() || !name.iter().copied().all(is_name) {
                return None;
            }
            at += close + 1;
            fd = Some(NEW_DESCRIPTOR);
        }
        let rest = &self.b[at..];
        let (op, length) = if rest.starts_with(b"<<<") {
            (RedirectOp::HereString, 3)
        } else if rest.starts_with(b"<<-") {
            (RedirectOp::HereDoc, 3)
        } else if rest.starts_with(b"<<") {
            (RedirectOp::HereDoc, 2)
        } else if rest.starts_with(b"<&") || rest.starts_with(b">&") {
            (RedirectOp::Duplicate, 2)
        } else if rest.starts_with(b"<>") {
            (RedirectOp::ReadWrite, 2)
        } else if rest.starts_with(b"<(") || rest.starts_with(b">(") {
            return None;
        } else if rest.starts_with(b"<") {
            (RedirectOp::Input, 1)
        } else if rest.starts_with(b">>") {
            (RedirectOp::Append, 2)
        } else if rest.starts_with(b">|") {
            (RedirectOp::Output, 2)
        } else if rest.starts_with(b">") {
            (RedirectOp::Output, 1)
        } else if at == start && rest.starts_with(b"&>>") {
            (RedirectOp::Append, 3)
        } else if at == start && rest.starts_with(b"&>") {
            (RedirectOp::Output, 2)
        } else {
            return None;
        };
        let strip_tabs = op == RedirectOp::HereDoc && rest.starts_with(b"<<-");
        self.pos = at + length;
        self.skip_blanks();
        let target = self.read_word().unwrap_or_else(|| {
            self.broken = true;
            Word::default()
        });
        let heredoc = (op == RedirectOp::HereDoc).then(|| {
            let index = self.bodies.len();
            self.bodies.push(None);
            let quoted = target
                .parts
                .iter()
                .any(|part| !matches!(part, Part::Literal(_, Quoting::Bare)));
            let delimiter: String = target
                .parts
                .iter()
                .map(|part| match part {
                    Part::Literal(text, _) => text.as_str(),
                    Part::Parameter { .. }
                    | Part::Command { .. }
                    | Part::Arithmetic(_)
                    | Part::Process { .. } => "",
                })
                .collect();
            let delimiter = if target.has_expansion() {
                target.raw.clone()
            } else {
                delimiter
            };
            self.pending.push(PendingHeredoc {
                index,
                delimiter,
                strip_tabs,
                quoted,
            });
            HereDoc {
                raw: String::new(),
                body: Word::default(),
                slot: Some(index),
            }
        });
        Some(Redirect {
            fd,
            op,
            target,
            heredoc,
        })
    }

    // ---- words ----------------------------------------------------------

    /// One word at the cursor, or `None` when the cursor is on a blank,
    /// newline, operator or the end.
    fn read_word(&mut self) -> Option<Word> {
        let start = self.pos;
        let mut parts = Vec::new();
        let mut text = String::new();
        let flush = |text: &mut String, parts: &mut Vec<Part>| {
            if !text.is_empty() {
                parts.push(Part::Literal(std::mem::take(text), Quoting::Bare));
            }
        };
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\n' => break,
                b'\r' if self.peek_at(1) == Some(b'\n') => break,
                b'<' | b'>' if self.peek_at(1) == Some(b'(') => {
                    flush(&mut text, &mut parts);
                    let input = byte == b'<';
                    self.pos += 2;
                    let body = self.nested(Stop::Paren);
                    self.expect_byte(b')');
                    parts.push(Part::Process { body, input });
                }
                b'(' if !text.is_empty() && text.ends_with(['@', '!', '+', '*', '?']) => {
                    // An extended glob: `@(a|b)`.
                    let close = self.balanced_paren_end(self.pos);
                    text.push_str(&self.src[self.pos..close]);
                    self.pos = close;
                }
                _ if is_meta(byte) => break,
                b'\\' => match self.peek_at(1) {
                    Some(b'\n') => self.pos += 2,
                    Some(b'\r') if self.peek_at(2) == Some(b'\n') => self.pos += 3,
                    Some(_) => {
                        flush(&mut text, &mut parts);
                        self.pos += 1;
                        let ch = self.take_char();
                        parts.push(Part::Literal(ch.to_string(), Quoting::Single));
                    }
                    None => {
                        self.pos += 1;
                        text.push('\\');
                    }
                },
                b'\'' => {
                    flush(&mut text, &mut parts);
                    self.pos += 1;
                    parts.push(Part::Literal(self.single_quoted(), Quoting::Single));
                }
                b'"' => {
                    flush(&mut text, &mut parts);
                    self.pos += 1;
                    let before = parts.len();
                    self.double_quoted(&mut parts);
                    if parts.len() == before {
                        parts.push(Part::Literal(String::new(), Quoting::Double));
                    }
                }
                b'`' => {
                    flush(&mut text, &mut parts);
                    self.pos += 1;
                    let body = self.backquoted();
                    parts.push(Part::Command {
                        body,
                        quoted: false,
                    });
                }
                b'$' => {
                    if let Some(part) = self.dollar(false) {
                        flush(&mut text, &mut parts);
                        match part {
                            Dollar::Part(part) => parts.push(part),
                            Dollar::Quoted(decoded) => {
                                parts.push(Part::Literal(decoded, Quoting::Single));
                            }
                            Dollar::Double => {
                                let before = parts.len();
                                self.double_quoted(&mut parts);
                                if parts.len() == before {
                                    parts.push(Part::Literal(String::new(), Quoting::Double));
                                }
                            }
                        }
                    } else {
                        self.pos += 1;
                        text.push('$');
                    }
                }
                _ => text.push(self.take_char()),
            }
        }
        flush(&mut text, &mut parts);
        (self.pos > start).then(|| Word {
            parts,
            raw: self.src[start..self.pos].to_string(),
        })
    }

    /// Where the parenthesised run opened at `open` ends (just past its
    /// `)`), or the text end.
    fn balanced_paren_end(&self, open: usize) -> usize {
        let mut depth = 0usize;
        let mut at = open;
        while at < self.b.len() {
            match self.b[at] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return at + 1;
                    }
                }
                b'\\' => at += 1,
                b'\n' => return at,
                _ => {}
            }
            at += 1;
        }
        self.b.len()
    }

    fn single_quoted(&mut self) -> String {
        let rest = &self.b[self.pos..];
        if let Some(close) = memchr::memchr(b'\'', rest) {
            let text = self.src[self.pos..self.pos + close].to_string();
            self.pos += close + 1;
            text
        } else {
            self.broken = true;
            let text = self.src[self.pos..].to_string();
            self.pos = self.b.len();
            text
        }
    }

    /// The parts of a `"..."` body (the cursor is past the opening quote).
    fn double_quoted(&mut self, parts: &mut Vec<Part>) {
        let mut text = String::new();
        let flush = |text: &mut String, parts: &mut Vec<Part>| {
            if !text.is_empty() {
                parts.push(Part::Literal(std::mem::take(text), Quoting::Double));
            }
        };
        loop {
            match self.peek() {
                None => {
                    self.broken = true;
                    break;
                }
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => match self.peek_at(1) {
                    Some(b'\n') => self.pos += 2,
                    Some(next @ (b'$' | b'`' | b'"' | b'\\')) => {
                        self.pos += 2;
                        text.push(char::from(next));
                    }
                    Some(_) | None => {
                        self.pos += 1;
                        text.push('\\');
                    }
                },
                Some(b'`') => {
                    flush(&mut text, parts);
                    self.pos += 1;
                    let body = self.backquoted();
                    parts.push(Part::Command { body, quoted: true });
                }
                Some(b'$') => match self.dollar(true) {
                    Some(Dollar::Part(part)) => {
                        flush(&mut text, parts);
                        parts.push(part);
                    }
                    Some(Dollar::Quoted(_) | Dollar::Double) | None => {
                        // `$'` and `$"` are literal inside double quotes.
                        self.pos += 1;
                        text.push('$');
                    }
                },
                Some(_) => text.push(self.take_char()),
            }
        }
        flush(&mut text, parts);
    }

    /// The command list of a backquoted substitution (the cursor is past the
    /// opening backquote).
    fn backquoted(&mut self) -> List {
        let mut inner = String::new();
        loop {
            match self.peek() {
                None => {
                    self.broken = true;
                    break;
                }
                Some(b'`') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    if let Some(next @ (b'$' | b'`' | b'\\')) = self.peek_at(1) {
                        self.pos += 2;
                        inner.push(char::from(next));
                    } else {
                        self.pos += 1;
                        inner.push('\\');
                    }
                }
                Some(_) => inner.push(self.take_char()),
            }
        }
        // Bash parses a backquoted command when it runs it: an error there
        // fails that substitution, not the line around it.
        let mut parsed = parse_at_depth(&inner, self.depth + 1);
        if let Some(dropped) = parsed.dropped {
            parsed.list.items.push(single(Command::Unparsed(dropped)));
        }
        parsed.list
    }

    /// A `$` form at the cursor, or `None` when the `$` is literal.
    fn dollar(&mut self, quoted: bool) -> Option<Dollar> {
        // Line continuations vanish before the shell tokenizes: `$\<nl>x`
        // is `$x`.
        let mut gap = 1;
        while self.b.get(self.pos + gap) == Some(&b'\\')
            && self.b.get(self.pos + gap + 1) == Some(&b'\n')
        {
            gap += 2;
        }
        if self.depth > MAX_DEPTH {
            // Nesting past the bound: the rest is kept unread.
            let rest = self.unparsed_rest();
            return Some(Dollar::Part(Part::Command {
                body: List {
                    items: vec![single(rest)],
                },
                quoted,
            }));
        }
        if gap > 1 {
            if self
                .b
                .get(self.pos + gap)
                .is_some_and(|&byte| is_name_start(byte) || byte == b'{' || byte == b'(')
            {
                self.pos += gap - 1;
            } else {
                return None;
            }
        }
        let next = self.peek_at(1)?;
        if quoted && (next == b'\'' || next == b'"') {
            // `$'` and `$"` are literal inside double quotes.
            return None;
        }
        match next {
            b'\'' => {
                self.pos += 2;
                Some(Dollar::Quoted(self.ansi_c()))
            }
            b'"' => {
                self.pos += 2;
                Some(Dollar::Double)
            }
            b'(' if self.peek_at(2) == Some(b'(') && self.arithmetic_closes(self.pos + 3) => {
                self.pos += 3;
                let expression = self.arithmetic_body();
                Some(Dollar::Part(Part::Arithmetic(Box::new(expression))))
            }
            b'(' => {
                self.pos += 2;
                self.substitutions += 1;
                let body = self.nested(Stop::Paren);
                self.substitutions -= 1;
                self.expect_byte(b')');
                Some(Dollar::Part(Part::Command { body, quoted }))
            }
            b'{' => {
                self.pos += 2;
                self.depth += 1;
                let part = self.braced_parameter(quoted);
                self.depth -= 1;
                Some(Dollar::Part(part))
            }
            b'[' => {
                // `$[expr]`: the old arithmetic form.
                self.pos += 2;
                let close = memchr::memchr(b']', &self.b[self.pos..])
                    .map_or(self.b.len(), |at| self.pos + at);
                let text = self.src[self.pos..close].to_string();
                self.pos = (close + 1).min(self.b.len());
                Some(Dollar::Part(Part::Arithmetic(Box::new(expand_text(
                    &text,
                    self.depth + 1,
                )))))
            }
            byte if is_name_start(byte) => {
                self.pos += 1;
                let start = self.pos;
                while self.peek().is_some_and(is_name) {
                    self.pos += 1;
                }
                Some(Dollar::Part(Part::Parameter {
                    name: self.src[start..self.pos].to_string(),
                    operator: None,
                    quoted,
                }))
            }
            byte if byte.is_ascii_digit() || b"@*#?-$!".contains(&byte) => {
                self.pos += 2;
                Some(Dollar::Part(Part::Parameter {
                    name: char::from(byte).to_string(),
                    operator: None,
                    quoted,
                }))
            }
            _ => None,
        }
    }

    /// The parameter name at the cursor inside `${...}`: a variable name, a
    /// positional number, or one special character.
    fn parameter_name(&mut self) -> String {
        let name_start = self.pos;
        match self.peek() {
            Some(byte) if is_name_start(byte) => {
                while self.peek().is_some_and(is_name) {
                    self.pos += 1;
                }
            }
            Some(byte) if byte.is_ascii_digit() => {
                while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            Some(byte) if b"@*#?-$!".contains(&byte) => self.pos += 1,
            Some(_) | None => {}
        }
        self.src[name_start..self.pos].to_string()
    }

    /// `${...}` (the cursor is past `${`).
    fn braced_parameter(&mut self, quoted: bool) -> Part {
        let start = self.pos;
        let mut prefix = String::new();
        if matches!(self.peek(), Some(b'#' | b'!')) && self.peek_at(1) != Some(b'}') {
            prefix.push(char::from(self.b[self.pos]));
            self.pos += 1;
        }
        let name_start = self.pos;
        let name = self.parameter_name();
        let operator_start = self.pos;
        let mut parts = Vec::new();
        let mut text = prefix;
        let flush = |text: &mut String, parts: &mut Vec<Part>| {
            if !text.is_empty() {
                parts.push(Part::Literal(std::mem::take(text), Quoting::Bare));
            }
        };
        let mut braces = 0usize;
        loop {
            match self.peek() {
                None => {
                    self.broken = true;
                    break;
                }
                Some(b'}') if braces == 0 => {
                    self.pos += 1;
                    break;
                }
                Some(b'}') => {
                    braces -= 1;
                    text.push('}');
                    self.pos += 1;
                }
                Some(b'{') => {
                    braces += 1;
                    text.push('{');
                    self.pos += 1;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    if !self.eof() {
                        text.push(self.take_char());
                    }
                }
                Some(b'\'') if !quoted => {
                    flush(&mut text, &mut parts);
                    self.pos += 1;
                    parts.push(Part::Literal(self.single_quoted(), Quoting::Single));
                }
                Some(b'"') => {
                    flush(&mut text, &mut parts);
                    self.pos += 1;
                    self.double_quoted(&mut parts);
                }
                Some(b'`') => {
                    flush(&mut text, &mut parts);
                    self.pos += 1;
                    let body = self.backquoted();
                    parts.push(Part::Command { body, quoted });
                }
                Some(b'$') => match self.dollar(quoted) {
                    Some(Dollar::Part(part)) => {
                        flush(&mut text, &mut parts);
                        parts.push(part);
                    }
                    Some(Dollar::Quoted(decoded)) => {
                        flush(&mut text, &mut parts);
                        parts.push(Part::Literal(decoded, Quoting::Single));
                    }
                    Some(Dollar::Double) => {
                        flush(&mut text, &mut parts);
                        self.double_quoted(&mut parts);
                    }
                    None => {
                        self.pos += 1;
                        text.push('$');
                    }
                },
                Some(_) => text.push(self.take_char()),
            }
        }
        flush(&mut text, &mut parts);
        let raw_end = self.pos.saturating_sub(1).max(operator_start);
        let operator = (!parts.is_empty() || start != name_start).then(|| {
            Box::new(Word {
                parts,
                raw: self.src[operator_start..raw_end].to_string(),
            })
        });
        Part::Parameter {
            name,
            operator,
            quoted,
        }
    }

    /// Whether the arithmetic opened just before `from` closes with `))`
    /// (bash reads `$((a) )` as a substitution of a subshell instead).
    fn arithmetic_closes(&self, from: usize) -> bool {
        let mut depth = 0usize;
        let mut at = from;
        while at < self.b.len() {
            match self.b[at] {
                b'(' => depth += 1,
                b')' if depth > 0 => depth -= 1,
                b')' => return self.b.get(at + 1) == Some(&b')'),
                b'\'' => {
                    at += memchr::memchr(b'\'', &self.b[at + 1..]).map_or(0, |close| close + 1);
                }
                _ => {}
            }
            at += 1;
        }
        true
    }

    /// The body of `$((...))` / `((...))` up to its `))` (the cursor is past
    /// the opening parentheses), parsed for the substitutions it runs.
    fn arithmetic_body(&mut self) -> Word {
        let start = self.pos;
        let mut depth = 0usize;
        let mut end = self.b.len();
        let mut at = self.pos;
        while at < self.b.len() {
            match self.b[at] {
                b'(' => depth += 1,
                b')' if depth > 0 => depth -= 1,
                b')' if self.b.get(at + 1) == Some(&b')') => {
                    end = at;
                    break;
                }
                b')' => {
                    end = at;
                    break;
                }
                b'\'' => {
                    at += memchr::memchr(b'\'', &self.b[at + 1..]).map_or(0, |close| close + 1);
                }
                _ => {}
            }
            at += 1;
        }
        let text = self.src[start..end].to_string();
        self.pos = if end >= self.b.len() {
            self.broken = true;
            self.b.len()
        } else {
            (end + 2).min(self.b.len())
        };
        expand_text(&text, self.depth + 1)
    }

    /// `$'...'` (the cursor is past `$'`), decoded.
    fn ansi_c(&mut self) -> String {
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                self.broken = true;
                break;
            };
            match byte {
                b'\'' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    self.ansi_escape(&mut out);
                }
                _ => out.push(self.take_char()),
            }
        }
        out
    }

    fn ansi_escape(&mut self, out: &mut String) {
        let Some(byte) = self.peek().filter(u8::is_ascii) else {
            // A backslash before nothing or a non-ASCII char stays literal.
            out.push('\\');
            return;
        };
        self.pos += 1;
        let simple = match byte {
            b'a' => Some('\x07'),
            b'b' => Some('\x08'),
            b'e' | b'E' => Some('\x1b'),
            b'f' => Some('\x0c'),
            b'n' => Some('\n'),
            b'r' => Some('\r'),
            b't' => Some('\t'),
            b'v' => Some('\x0b'),
            b'\\' => Some('\\'),
            b'\'' => Some('\''),
            b'"' => Some('"'),
            b'?' => Some('?'),
            _ => None,
        };
        if let Some(ch) = simple {
            out.push(ch);
            return;
        }
        let digits = |parser: &mut Self, max: usize, radix: u32| -> Option<u32> {
            let start = parser.pos;
            while parser.pos - start < max
                && parser
                    .peek()
                    .is_some_and(|byte| char::from(byte).is_digit(radix))
            {
                parser.pos += 1;
            }
            (parser.pos > start)
                .then(|| u32::from_str_radix(&parser.src[start..parser.pos], radix).ok())
                .flatten()
        };
        let code = match byte {
            b'x' => digits(self, 2, 16),
            b'u' => digits(self, 4, 16),
            b'U' => digits(self, 8, 16),
            b'0'..=b'7' => {
                self.pos -= 1;
                digits(self, 3, 8)
            }
            b'c' => self.take_control(),
            _ => None,
        };
        if let Some(ch) = code.and_then(char::from_u32) {
            out.push(ch);
        } else {
            // Not a valid escape (or no code point): bash keeps it.
            out.push('\\');
            out.push(char::from(byte));
        }
    }

    fn take_control(&mut self) -> Option<u32> {
        let ch = self.take_char();
        ch.is_ascii()
            .then(|| u32::from(ch.to_ascii_uppercase() as u8 ^ 0x40))
    }
}

enum Dollar {
    Part(Part),
    /// `$'...'`, decoded.
    Quoted(String),
    /// `$"..."`: read as a double-quoted string.
    Double,
}

fn single(command: Command) -> AndOr {
    AndOr {
        first: Pipeline {
            negated: false,
            commands: vec![command],
        },
        rest: Vec::new(),
        background: false,
    }
}

/// Text read the way an unquoted here-document body (or an arithmetic
/// expression) is: `$` expansions, backquotes and backslash escapes live,
/// quote characters literal.
pub(crate) fn expand_text(text: &str, depth: usize) -> Word {
    let mut parser = Parser::new(text, depth);
    let mut parts = Vec::new();
    let mut literal = String::new();
    let flush = |literal: &mut String, parts: &mut Vec<Part>| {
        if !literal.is_empty() {
            parts.push(Part::Literal(std::mem::take(literal), Quoting::Double));
        }
    };
    while let Some(byte) = parser.peek() {
        match byte {
            b'\\' => match parser.peek_at(1) {
                Some(b'\n') => parser.pos += 2,
                Some(next @ (b'$' | b'`' | b'\\')) => {
                    parser.pos += 2;
                    literal.push(char::from(next));
                }
                Some(_) | None => {
                    parser.pos += 1;
                    literal.push('\\');
                }
            },
            b'`' => {
                flush(&mut literal, &mut parts);
                parser.pos += 1;
                let body = parser.backquoted();
                parts.push(Part::Command { body, quoted: true });
            }
            b'$' => match parser.dollar(true) {
                Some(Dollar::Part(part)) => {
                    flush(&mut literal, &mut parts);
                    parts.push(part);
                }
                Some(Dollar::Quoted(_) | Dollar::Double) | None => {
                    parser.pos += 1;
                    literal.push('$');
                }
            },
            _ => literal.push(parser.take_char()),
        }
    }
    flush(&mut literal, &mut parts);
    let mut word = Word {
        parts,
        raw: text.to_string(),
    };
    // Here-documents opened inside substitutions of the body belong to them.
    let mut holder = List {
        items: vec![single(Command::Simple(SimpleCommand {
            words: vec![std::mem::take(&mut word)],
            ..SimpleCommand::default()
        }))],
    };
    parser.fill_heredocs(&mut holder);
    match holder.items.pop().map(|item| item.first.commands) {
        Some(mut commands) => match commands.pop() {
            Some(Command::Simple(mut command)) => command.words.pop().unwrap_or_default(),
            Some(_) | None => Word::default(),
        },
        None => Word::default(),
    }
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
