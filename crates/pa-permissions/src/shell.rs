//! Shell command splitting for Bash rules: a command line becomes the
//! simple commands it runs (split on `&&`, `||`, `;`, `|`, `|&`, `&`,
//! newlines and subshell parentheses, quote-aware), each with its words and
//! redirections. Claude Code checks a compound command one part at a time:
//! a deny or ask rule that matches any part applies to the whole, and an
//! allow rule must cover every part. Command and process substitutions
//! (`$(...)`, backticks, `<(...)`) and heredocs make a command unverifiable
//! (no allow rule approves it), and their inner commands are checked as
//! parts too.

/// One word of a simple command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Word {
    /// The word as written (quotes and escapes kept).
    pub raw: String,
    /// The word's value (quotes removed, escapes resolved).
    pub value: String,
}

/// One simple command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimpleCommand {
    /// Its text as written, redirections included.
    pub text: String,
    /// Its words, redirections excluded.
    pub words: Vec<Word>,
    /// Files its output redirections write (`> f`, `>> f`, `&> f`, `2> f`).
    pub writes: Vec<String>,
    /// Files its input redirections read (`< f`).
    pub reads: Vec<String>,
}

impl SimpleCommand {
    /// The command without its redirections, words joined by one space
    /// (Claude Code's `commandWithoutRedirections`).
    #[must_use]
    pub fn without_redirections(&self) -> String {
        join_raw(&self.words)
    }

    /// The words after leading environment assignments and the transparent
    /// wrappers (`env`, `timeout`, `time`, `nice`, `nohup`, `stdbuf`) when
    /// `strip_assignments`; after the wrappers alone otherwise (an allow rule
    /// must not approve `PATH=... cmd` as `cmd`).
    #[must_use]
    pub fn core_words(&self, strip_assignments: bool) -> &[Word] {
        let mut words: &[Word] = &self.words;
        loop {
            let before = words.len();
            if strip_assignments {
                while words.first().is_some_and(|word| is_assignment(&word.raw)) {
                    words = &words[1..];
                }
            }
            words = strip_wrapper(words, strip_assignments);
            if words.len() == before {
                return words;
            }
        }
    }

    /// The command's name after the wrappers and assignments.
    #[must_use]
    pub fn program(&self) -> Option<&str> {
        self.core_words(true)
            .first()
            .map(|word| word.value.rsplit('/').next().unwrap_or(&word.value))
    }
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedCommand {
    /// Its simple commands in order, those of substitutions after the
    /// command they appear in.
    pub commands: Vec<SimpleCommand>,
    /// It holds what the split cannot verify: a command or process
    /// substitution, a heredoc, an unterminated quote.
    pub unverifiable: bool,
}

/// The words joined by one space, as written.
#[must_use]
pub fn join_raw(words: &[Word]) -> String {
    words
        .iter()
        .map(|word| word.raw.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `NAME=value`.
fn is_assignment(raw: &str) -> bool {
    let Some((name, _)) = raw.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// `words` past one leading wrapper and its options, unchanged when the
/// first word is not a wrapper.
fn strip_wrapper(words: &[Word], strip_assignments: bool) -> &[Word] {
    let Some(first) = words.first() else {
        return words;
    };
    let rest = &words[1..];
    let skip_options = |mut rest: &'_ [Word], with_value: &[&str]| -> usize {
        let start = rest.len();
        while let Some(word) = rest.first() {
            if !word.value.starts_with('-') || word.value == "-" {
                break;
            }
            let takes_value = with_value.contains(&word.value.as_str());
            rest = &rest[if takes_value { 2.min(rest.len()) } else { 1 }..];
        }
        start - rest.len()
    };
    let skipped = match first.value.as_str() {
        // `env` runs its command with assignments; they count as the
        // command's own only where assignments are stripped.
        "env" => {
            let options = skip_options(rest, &["-u", "--unset", "-C", "--chdir"]);
            let assignments = rest[options..]
                .iter()
                .take_while(|word| is_assignment(&word.raw))
                .count();
            if assignments > 0 && !strip_assignments {
                return words;
            }
            options + assignments
        }
        "timeout" => {
            let options = skip_options(rest, &["-s", "--signal", "-k", "--kill-after"]);
            // The duration.
            options + usize::from(rest.len() > options)
        }
        "time" | "nohup" => skip_options(rest, &[]),
        "nice" => skip_options(rest, &["-n", "--adjustment"]),
        "stdbuf" => skip_options(rest, &["-i", "-o", "-e"]),
        _ => return words,
    };
    &rest[skipped.min(rest.len())..]
}

/// A redirection operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redirect {
    /// `>`, `>>`, `>|`, `&>`, `&>>`: writes its target.
    Write,
    /// `<`: reads its target.
    Read,
    /// `<>`: reads and writes its target.
    ReadWrite,
    /// `>&`, `<&`: duplicates a descriptor (or, before a non-number, writes a
    /// file).
    Duplicate,
    /// `<<`, `<<-`: a heredoc (its body is unverifiable).
    Heredoc,
    /// `<<<`: a here-string (its word is data).
    HereString,
}

/// The redirection operator at the start of `chars[at..]`, with the length
/// it takes.
fn redirect_at(chars: &[char], at: usize) -> Option<(Redirect, usize)> {
    let get = |offset: usize| chars.get(at + offset).copied();
    match (get(0), get(1), get(2)) {
        (Some('&'), Some('>'), Some('>')) => Some((Redirect::Write, 3)),
        (Some('&'), Some('>'), _) | (Some('>'), Some('>' | '|'), _) => Some((Redirect::Write, 2)),
        (Some('>' | '<'), Some('&'), _) => Some((Redirect::Duplicate, 2)),
        (Some('>'), _, _) => Some((Redirect::Write, 1)),
        (Some('<'), Some('<'), Some('<')) => Some((Redirect::HereString, 3)),
        (Some('<'), Some('<'), Some('-')) => Some((Redirect::Heredoc, 3)),
        (Some('<'), Some('<'), _) => Some((Redirect::Heredoc, 2)),
        (Some('<'), Some('>'), _) => Some((Redirect::ReadWrite, 2)),
        (Some('<'), _, _) => Some((Redirect::Read, 1)),
        _ => None,
    }
}

/// The lexer's output before commands are assembled.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(Word),
    Redirect(Redirect),
    /// A command separator (an operator, a newline, a parenthesis).
    Separator,
}

struct Lexer<'a> {
    chars: &'a [char],
    at: usize,
    unverifiable: bool,
    /// Inner command lines of substitutions, parsed after the line.
    substitutions: Vec<String>,
}

impl Lexer<'_> {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    /// Read a balanced `(...)` body starting after the opening parenthesis,
    /// quote-aware; returns the body.
    fn balanced_body(&mut self) -> String {
        let start = self.at;
        let mut depth = 1usize;
        let mut quote: Option<char> = None;
        while let Some(ch) = self.peek(0) {
            self.at += 1;
            match (quote, ch) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (Some('"') | None, '\\') => self.at += 1,
                (None, '\'' | '"') => quote = Some(ch),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        return self.chars[start..self.at - 1].iter().collect();
                    }
                }
                // Quoted text, and anything else unquoted.
                (Some(_) | None, _) => {}
            }
        }
        self.unverifiable = true;
        self.chars[start.min(self.chars.len())..].iter().collect()
    }

    /// Read a backtick body after the opening backtick.
    fn backtick_body(&mut self) -> String {
        let start = self.at;
        while let Some(ch) = self.peek(0) {
            self.at += 1;
            match ch {
                '\\' => self.at += 1,
                '`' => return self.chars[start..self.at - 1].iter().collect(),
                _ => {}
            }
        }
        self.unverifiable = true;
        self.chars[start.min(self.chars.len())..].iter().collect()
    }

    /// A `$(`, `<(`, `>(` or backtick substitution at the cursor: consumes
    /// it into `raw` and records its body.
    fn substitution(&mut self, raw: &mut String, value: &mut String) -> bool {
        let (open, skip) = match (self.peek(0), self.peek(1), self.peek(2)) {
            // `$((` is arithmetic: kept as data.
            (Some('$'), Some('('), Some('(')) => return false,
            (Some('$' | '<' | '>'), Some('('), _) => ("(", 2),
            (Some('`'), _, _) => ("`", 1),
            _ => return false,
        };
        let start = self.at;
        self.at += skip;
        let body = if open == "(" {
            self.balanced_body()
        } else {
            self.backtick_body()
        };
        let text: String = self.chars[start..self.at.min(self.chars.len())]
            .iter()
            .collect();
        raw.push_str(&text);
        value.push_str(&text);
        self.unverifiable = true;
        self.substitutions.push(body);
        true
    }

    /// Read a single-quoted span at the cursor (literal to the next quote).
    fn single_quoted(&mut self, raw: &mut String, value: &mut String) {
        raw.push('\'');
        self.at += 1;
        loop {
            match self.peek(0) {
                Some('\'') => {
                    raw.push('\'');
                    self.at += 1;
                    return;
                }
                Some(inner) => {
                    raw.push(inner);
                    value.push(inner);
                    self.at += 1;
                }
                None => {
                    self.unverifiable = true;
                    return;
                }
            }
        }
    }

    /// Read a double-quoted span at the cursor (backslash escapes `"`, `\`,
    /// `$` and backtick; substitutions inside are followed).
    fn double_quoted(&mut self, raw: &mut String, value: &mut String) {
        raw.push('"');
        self.at += 1;
        loop {
            match self.peek(0) {
                Some('"') => {
                    raw.push('"');
                    self.at += 1;
                    return;
                }
                Some('\\') => {
                    raw.push('\\');
                    self.at += 1;
                    if let Some(next) = self.peek(0) {
                        raw.push(next);
                        if !matches!(next, '"' | '\\' | '$' | '`') {
                            value.push('\\');
                        }
                        value.push(next);
                        self.at += 1;
                    }
                }
                Some('$' | '`') if self.substitution(raw, value) => {}
                Some(inner) => {
                    raw.push(inner);
                    value.push(inner);
                    self.at += 1;
                }
                None => {
                    self.unverifiable = true;
                    return;
                }
            }
        }
    }

    /// Read a `$((...))` arithmetic expansion at the cursor: data, kept
    /// whole.
    fn arithmetic(&mut self, raw: &mut String, value: &mut String) {
        let start = self.at;
        self.at += 3;
        let mut depth = 2usize;
        while let Some(inner) = self.peek(0) {
            self.at += 1;
            match inner {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        let text: String = self.chars[start..self.at.min(self.chars.len())]
            .iter()
            .collect();
        raw.push_str(&text);
        value.push_str(&text);
    }

    /// Read one word at the cursor (not at whitespace or an operator).
    fn word(&mut self) -> Word {
        let mut raw = String::new();
        let mut value = String::new();
        while let Some(ch) = self.peek(0) {
            match ch {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => {
                    // `<(`/`>(` inside a word is a process substitution.
                    if matches!(ch, '<' | '>') && self.peek(1) == Some('(') {
                        self.substitution(&mut raw, &mut value);
                        continue;
                    }
                    break;
                }
                '\\' => {
                    raw.push(ch);
                    self.at += 1;
                    if let Some(next) = self.peek(0) {
                        raw.push(next);
                        if next != '\n' {
                            value.push(next);
                        }
                        self.at += 1;
                    }
                }
                '\'' => self.single_quoted(&mut raw, &mut value),
                '"' => self.double_quoted(&mut raw, &mut value),
                '$' if self.peek(1) == Some('(') && self.peek(2) == Some('(') => {
                    self.arithmetic(&mut raw, &mut value);
                }
                '$' | '`' if self.substitution(&mut raw, &mut value) => {}
                _ => {
                    raw.push(ch);
                    value.push(ch);
                    self.at += 1;
                }
            }
        }
        Word { raw, value }
    }

    /// The tokens, each with its character span in the line.
    fn tokens(&mut self) -> Vec<(Token, usize, usize)> {
        let mut tokens = Vec::new();
        while let Some(ch) = self.peek(0) {
            let start = self.at;
            match ch {
                ' ' | '\t' => self.at += 1,
                '\\' if self.peek(1) == Some('\n') => self.at += 2,
                '\n' | ';' | '(' | ')' => {
                    // `;;` and friends are separators too.
                    self.at += 1;
                    tokens.push((Token::Separator, start, self.at));
                }
                '#' => {
                    while self.peek(0).is_some_and(|ch| ch != '\n') {
                        self.at += 1;
                    }
                }
                '|' => {
                    self.at += if matches!(self.peek(1), Some('|' | '&')) {
                        2
                    } else {
                        1
                    };
                    tokens.push((Token::Separator, start, self.at));
                }
                '&' if self.peek(1) != Some('>') => {
                    self.at += if self.peek(1) == Some('&') { 2 } else { 1 };
                    tokens.push((Token::Separator, start, self.at));
                }
                '<' | '>' if self.peek(1) == Some('(') => {
                    let word = self.word();
                    tokens.push((Token::Word(word), start, self.at));
                }
                '<' | '>' | '&' => {
                    if let Some((redirect, length)) = redirect_at(self.chars, self.at) {
                        self.at += length;
                        tokens.push((Token::Redirect(redirect), start, self.at));
                    } else {
                        self.at += 1;
                    }
                }
                _ => {
                    let word = self.word();
                    // A descriptor number glued to a redirection (`2>`): the
                    // redirection's span starts at it.
                    if !word.raw.is_empty()
                        && word.raw.chars().all(|ch| ch.is_ascii_digit())
                        && matches!(self.peek(0), Some('<' | '>'))
                    {
                        if let Some((redirect, length)) = redirect_at(self.chars, self.at) {
                            self.at += length;
                            tokens.push((Token::Redirect(redirect), start, self.at));
                        }
                        continue;
                    }
                    tokens.push((Token::Word(word), start, self.at));
                }
            }
        }
        tokens
    }
}

/// Assemble the simple commands of one token run over `chars`.
fn assemble(
    chars: &[char],
    tokens: Vec<(Token, usize, usize)>,
    unverifiable: &mut bool,
) -> Vec<SimpleCommand> {
    let mut commands = Vec::new();
    let mut current = SimpleCommand::default();
    // The current command's character span.
    let mut span: Option<(usize, usize)> = None;
    let mut tokens = tokens.into_iter().peekable();
    let finish = |current: &mut SimpleCommand,
                  span: &mut Option<(usize, usize)>,
                  commands: &mut Vec<SimpleCommand>| {
        let mut command = std::mem::take(current);
        if let Some((start, end)) = span.take() {
            command.text = chars[start..end]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
        }
        // Group braces are syntax, not words.
        command
            .words
            .retain(|word| word.raw != "{" && word.raw != "}");
        if !command.words.is_empty() || !command.writes.is_empty() || !command.reads.is_empty() {
            commands.push(command);
        }
    };
    let extend = |span: &mut Option<(usize, usize)>, start: usize, end: usize| {
        *span = Some(span.map_or((start, end), |(first, _)| (first, end)));
    };
    while let Some((token, start, end)) = tokens.next() {
        match token {
            Token::Separator => finish(&mut current, &mut span, &mut commands),
            Token::Word(word) => {
                extend(&mut span, start, end);
                current.words.push(word);
            }
            Token::Redirect(redirect) => {
                extend(&mut span, start, end);
                if redirect == Redirect::Heredoc {
                    *unverifiable = true;
                }
                let target = match tokens.peek() {
                    Some((Token::Word(word), _, target_end)) => Some((word.clone(), *target_end)),
                    _ => None,
                };
                let Some((target, target_end)) = target else {
                    continue;
                };
                tokens.next();
                extend(&mut span, start, target_end);
                match redirect {
                    Redirect::Write => current.writes.push(target.value),
                    Redirect::Read => current.reads.push(target.value),
                    Redirect::ReadWrite => {
                        current.reads.push(target.value.clone());
                        current.writes.push(target.value);
                    }
                    Redirect::Duplicate => {
                        let fd = target.value.chars().all(|ch| ch.is_ascii_digit())
                            || target.value == "-";
                        if !fd {
                            current.writes.push(target.value);
                        }
                    }
                    Redirect::Heredoc | Redirect::HereString => {}
                }
            }
        }
    }
    finish(&mut current, &mut span, &mut commands);
    commands
}

/// The most nested substitutions followed.
const MAX_SUBSTITUTION_DEPTH: usize = 8;

/// Split `command` into the simple commands it runs.
#[must_use]
pub fn parse(command: &str) -> ParsedCommand {
    parse_at(command, 0)
}

fn parse_at(command: &str, depth: usize) -> ParsedCommand {
    let chars: Vec<char> = command.chars().collect();
    let mut lexer = Lexer {
        chars: &chars,
        at: 0,
        unverifiable: false,
        substitutions: Vec::new(),
    };
    let tokens = lexer.tokens();
    let mut unverifiable = lexer.unverifiable;
    let mut commands = assemble(&chars, tokens, &mut unverifiable);
    for inner in lexer.substitutions {
        if depth >= MAX_SUBSTITUTION_DEPTH {
            break;
        }
        let parsed = parse_at(&inner, depth + 1);
        commands.extend(parsed.commands);
    }
    ParsedCommand {
        commands,
        unverifiable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(command: &str) -> Vec<String> {
        parse(command)
            .commands
            .iter()
            .map(SimpleCommand::without_redirections)
            .collect()
    }

    #[test]
    fn compound_commands_split_on_every_operator() {
        let table: &[(&str, &[&str])] = &[
            ("git status", &["git status"]),
            (
                "git add . && git commit -m 'x y'",
                &["git add .", "git commit -m 'x y'"],
            ),
            ("a || b; c | d |& e & f", &["a", "b", "c", "d", "e", "f"]),
            ("a\nb", &["a", "b"]),
            ("(cd sub && make)", &["cd sub", "make"]),
            ("{ a; b; }", &["a", "b"]),
            ("echo 'a && b' \"c; d\"", &["echo 'a && b' \"c; d\""]),
            ("echo a\\;b", &["echo a\\;b"]),
            ("ls # && rm -rf /", &["ls"]),
        ];
        for (command, expected) in table {
            assert_eq!(texts(command), *expected, "{command}");
            assert!(!parse(command).unverifiable, "{command}");
        }
    }

    #[test]
    fn substitutions_are_unverifiable_and_their_commands_are_parts() {
        let parsed = parse("echo $(rm -rf /tmp/x) `whoami` <(cat a)");
        assert!(parsed.unverifiable);
        assert_eq!(
            parsed
                .commands
                .iter()
                .map(SimpleCommand::without_redirections)
                .collect::<Vec<_>>(),
            vec![
                "echo $(rm -rf /tmp/x) `whoami` <(cat a)",
                "rm -rf /tmp/x",
                "whoami",
                "cat a"
            ]
        );
        assert!(parse("echo \"$(id)\"").unverifiable);
        assert!(!parse("echo $((1 + 2))").unverifiable);
        assert!(parse("cat <<EOF\nhi\nEOF").unverifiable);
        assert!(parse("echo 'unterminated").unverifiable);
    }

    #[test]
    fn redirections_are_split_off() {
        let parsed = parse("npm test > out.log 2>&1 < in.txt");
        let command = &parsed.commands[0];
        assert_eq!(command.without_redirections(), "npm test");
        assert_eq!(command.writes, vec!["out.log"]);
        assert_eq!(command.reads, vec!["in.txt"]);
        assert_eq!(command.text, "npm test > out.log 2>&1 < in.txt");
        let parsed = parse("make &> 'build log.txt' && cat x >>y");
        assert_eq!(parsed.commands[0].writes, vec!["build log.txt"]);
        assert_eq!(parsed.commands[1].writes, vec!["y"]);
        assert_eq!(parsed.commands[1].without_redirections(), "cat x");
        assert_eq!(
            parse("echo hi >&2").commands[0].writes,
            Vec::<String>::new()
        );
    }

    #[test]
    fn wrappers_and_assignments_strip_to_the_core_command() {
        let parsed = parse("FOO=1 timeout 5 nice -n 10 env BAR=2 npm test");
        let command = &parsed.commands[0];
        assert_eq!(join_raw(command.core_words(true)), "npm test");
        // Without stripping assignments, the leading one stays.
        assert_eq!(
            join_raw(command.core_words(false)),
            "FOO=1 timeout 5 nice -n 10 env BAR=2 npm test"
        );
        assert_eq!(command.program(), Some("npm"));
        let parsed = parse("timeout -s KILL 30 /usr/bin/make all");
        assert_eq!(parsed.commands[0].program(), Some("make"));
        assert_eq!(
            join_raw(parsed.commands[0].core_words(false)),
            "/usr/bin/make all"
        );
    }

    #[test]
    fn word_values_resolve_quotes_and_escapes() {
        let parsed = parse(r#"rm "a b" 'c d' e\ f "g\"h""#);
        let values: Vec<&str> = parsed.commands[0]
            .words
            .iter()
            .map(|word| word.value.as_str())
            .collect();
        assert_eq!(values, vec!["rm", "a b", "c d", "e f", "g\"h"]);
    }
}
