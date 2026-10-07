//! A small backtracking matcher with Python `re` semantics, for the guard's
//! fixed patterns.
//!
//! The discard patterns rely on ordered alternation, greedy backtracking,
//! lookahead and Unicode `\s`/`\b` exactly as Python reads them, and the
//! verdicts must match the Python guard's byte for byte, so the patterns are
//! kept in their original spelling and run on this engine: a compiled program
//! of instructions executed with an explicit backtrack stack (no recursion, so
//! a 64 KiB input cannot exhaust the thread stack). It supports what the
//! guard's patterns use: literals and escapes, classes (ranges, negation,
//! `\s`/`\S`), groups (capturing, `(?:...)`, `(?=...)`), alternation, the
//! greedy quantifiers `*`, `+`, `?`, `{m,n}`, and the anchors `^`, `$`, `\b`.
//! Text is a slice of `char`s, so positions are Python string indices.

/// Python's `str.isspace()` (and `\s` in a `str` pattern): Unicode
/// whitespace plus the ASCII separators `\x1c`-`\x1f`.
pub(super) fn is_space(ch: char) -> bool {
    ch.is_whitespace() || ('\x1c'..='\x1f').contains(&ch)
}

/// A `\w` character in a Python `str` pattern.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[derive(Debug, Clone)]
enum ClassItem {
    Range(char, char),
    Space,
    NotSpace,
}

#[derive(Debug, Clone)]
struct Class {
    negated: bool,
    items: Vec<ClassItem>,
}

impl Class {
    fn matches(&self, ch: char) -> bool {
        let hit = self.items.iter().any(|item| match item {
            ClassItem::Range(low, high) => (*low..=*high).contains(&ch),
            ClassItem::Space => is_space(ch),
            ClassItem::NotSpace => !is_space(ch),
        });
        hit != self.negated
    }
}

#[derive(Debug, Clone)]
enum Node {
    Class(Class),
    Group(Box<Node>, Option<usize>),
    LookAhead(Box<Node>),
    Concat(Vec<Node>),
    Alternate(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>),
    Start,
    End,
    WordBoundary,
}

#[derive(Debug, Clone)]
enum Inst {
    Class(usize),
    /// Try the first target, backtrack into the second.
    Split(usize, usize),
    Jump(usize),
    Save(usize),
    LookAhead(usize),
    Start,
    End,
    WordBoundary,
    Match,
}

/// A compiled pattern. Construction panics on syntax it does not support:
/// every pattern is a constant, and the guard's tests compile all of them.
#[derive(Debug)]
pub(super) struct Pattern {
    programs: Vec<Vec<Inst>>,
    classes: Vec<Class>,
    groups: usize,
}

/// One match: the span of the whole match and of each capturing group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Captures {
    spans: Vec<Option<(usize, usize)>>,
}

impl Captures {
    pub(super) fn start(&self) -> usize {
        self.spans[0].map_or(0, |(start, _)| start)
    }

    pub(super) fn end(&self) -> usize {
        self.spans[0].map_or(0, |(_, end)| end)
    }

    /// The span of group `index` when it took part in the match.
    pub(super) fn group(&self, index: usize) -> Option<(usize, usize)> {
        self.spans.get(index).copied().flatten()
    }

    /// The text of group `index` when it took part in the match.
    pub(super) fn text(&self, text: &[char], index: usize) -> Option<String> {
        self.group(index)
            .map(|(start, end)| text[start..end].iter().collect())
    }
}

struct Parser<'a> {
    chars: Vec<char>,
    at: usize,
    groups: usize,
    _source: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> char {
        let ch = self.chars[self.at];
        self.at += 1;
        ch
    }

    fn alternation(&mut self) -> Node {
        let mut branches = vec![self.concat()];
        while self.peek() == Some('|') {
            self.at += 1;
            branches.push(self.concat());
        }
        if branches.len() == 1 {
            branches.pop().expect("one branch")
        } else {
            Node::Alternate(branches)
        }
    }

    fn concat(&mut self) -> Node {
        let mut items = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == '|' || ch == ')' {
                break;
            }
            let atom = self.atom();
            items.push(self.quantified(atom));
        }
        Node::Concat(items)
    }

    fn quantified(&mut self, atom: Node) -> Node {
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => {
                let close = self.chars[self.at..]
                    .iter()
                    .position(|ch| *ch == '}')
                    .expect("closed repeat");
                let body: String = self.chars[self.at + 1..self.at + close].iter().collect();
                self.at += close;
                let (low, high) = if let Some((low, high)) = body.split_once(',') {
                    (
                        low.parse().expect("repeat minimum"),
                        Some(high.parse().expect("repeat maximum")),
                    )
                } else {
                    let exact = body.parse().expect("repeat count");
                    (exact, Some(exact))
                };
                (low, high)
            }
            _ => return atom,
        };
        self.at += 1;
        Node::Repeat(Box::new(atom), min, max)
    }

    fn atom(&mut self) -> Node {
        match self.next() {
            '(' => {
                let (lookahead, capture) = if self.chars[self.at..].starts_with(&['?', ':']) {
                    self.at += 2;
                    (false, None)
                } else if self.chars[self.at..].starts_with(&['?', '=']) {
                    self.at += 2;
                    (true, None)
                } else {
                    self.groups += 1;
                    (false, Some(self.groups))
                };
                let inner = self.alternation();
                assert_eq!(self.next(), ')', "closed group");
                if lookahead {
                    Node::LookAhead(Box::new(inner))
                } else {
                    Node::Group(Box::new(inner), capture)
                }
            }
            '[' => Node::Class(self.class()),
            '^' => Node::Start,
            '$' => Node::End,
            '.' => panic!("`.` is not supported"),
            '\\' => match self.next() {
                'b' => Node::WordBoundary,
                's' => Node::Class(Class {
                    negated: false,
                    items: vec![ClassItem::Space],
                }),
                'S' => Node::Class(Class {
                    negated: false,
                    items: vec![ClassItem::NotSpace],
                }),
                escaped => literal(escape_char(escaped)),
            },
            ch => literal(ch),
        }
    }

    fn class(&mut self) -> Class {
        let negated = self.peek() == Some('^');
        if negated {
            self.at += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let ch = self.next();
            if ch == ']' && !first {
                break;
            }
            first = false;
            let low = if ch == '\\' {
                match self.next() {
                    's' => {
                        items.push(ClassItem::Space);
                        continue;
                    }
                    'S' => {
                        items.push(ClassItem::NotSpace);
                        continue;
                    }
                    escaped => escape_char(escaped),
                }
            } else {
                ch
            };
            if self.peek() == Some('-') && self.chars.get(self.at + 1).is_some_and(|c| *c != ']') {
                self.at += 1;
                let high = match self.next() {
                    '\\' => escape_char(self.next()),
                    high => high,
                };
                items.push(ClassItem::Range(low, high));
            } else {
                items.push(ClassItem::Range(low, low));
            }
        }
        Class { negated, items }
    }
}

fn escape_char(ch: char) -> char {
    match ch {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        other => other,
    }
}

fn literal(ch: char) -> Node {
    Node::Class(Class {
        negated: false,
        items: vec![ClassItem::Range(ch, ch)],
    })
}

struct Compiler {
    programs: Vec<Vec<Inst>>,
    classes: Vec<Class>,
}

impl Compiler {
    fn emit(&mut self, program: usize, node: &Node) {
        match node {
            Node::Class(class) => {
                self.classes.push(class.clone());
                let index = self.classes.len() - 1;
                self.programs[program].push(Inst::Class(index));
            }
            Node::Group(inner, capture) => {
                if let Some(group) = capture {
                    self.programs[program].push(Inst::Save(group * 2));
                }
                self.emit(program, inner);
                if let Some(group) = capture {
                    self.programs[program].push(Inst::Save(group * 2 + 1));
                }
            }
            Node::LookAhead(inner) => {
                self.programs.push(Vec::new());
                let sub = self.programs.len() - 1;
                self.emit(sub, inner);
                self.programs[sub].push(Inst::Match);
                self.programs[program].push(Inst::LookAhead(sub));
            }
            Node::Concat(items) => {
                for item in items {
                    self.emit(program, item);
                }
            }
            Node::Alternate(branches) => {
                let mut jumps = Vec::new();
                for (index, branch) in branches.iter().enumerate() {
                    if index + 1 < branches.len() {
                        let split = self.programs[program].len();
                        self.programs[program].push(Inst::Split(split + 1, 0));
                        self.emit(program, branch);
                        jumps.push(self.programs[program].len());
                        self.programs[program].push(Inst::Jump(0));
                        let next = self.programs[program].len();
                        self.programs[program][split] = Inst::Split(split + 1, next);
                    } else {
                        self.emit(program, branch);
                    }
                }
                let end = self.programs[program].len();
                for jump in jumps {
                    self.programs[program][jump] = Inst::Jump(end);
                }
            }
            Node::Repeat(inner, min, max) => {
                for _ in 0..*min {
                    self.emit(program, inner);
                }
                match max {
                    None => {
                        let split = self.programs[program].len();
                        self.programs[program].push(Inst::Split(split + 1, 0));
                        self.emit(program, inner);
                        self.programs[program].push(Inst::Jump(split));
                        let end = self.programs[program].len();
                        self.programs[program][split] = Inst::Split(split + 1, end);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.programs[program].len());
                            self.programs[program].push(Inst::Split(0, 0));
                            self.emit(program, inner);
                        }
                        let end = self.programs[program].len();
                        for split in splits {
                            self.programs[program][split] = Inst::Split(split + 1, end);
                        }
                    }
                }
            }
            Node::Start => self.programs[program].push(Inst::Start),
            Node::End => self.programs[program].push(Inst::End),
            Node::WordBoundary => self.programs[program].push(Inst::WordBoundary),
        }
    }
}

enum Frame {
    Try(usize, usize),
    Restore(usize, Option<usize>),
}

impl Pattern {
    pub(super) fn new(source: &str) -> Self {
        let mut parser = Parser {
            chars: source.chars().collect(),
            at: 0,
            groups: 0,
            _source: source,
        };
        let node = parser.alternation();
        assert!(
            parser.at == parser.chars.len(),
            "unbalanced pattern {source}"
        );
        let mut compiler = Compiler {
            programs: vec![Vec::new()],
            classes: Vec::new(),
        };
        compiler.programs[0].push(Inst::Save(0));
        compiler.emit(0, &node);
        compiler.programs[0].push(Inst::Save(1));
        compiler.programs[0].push(Inst::Match);
        Self {
            programs: compiler.programs,
            classes: compiler.classes,
            groups: parser.groups,
        }
    }

    /// Run program `program` anchored at `start`; `full` requires the match
    /// to end at the end of `text` (`re.fullmatch`).
    fn run(
        &self,
        program: usize,
        text: &[char],
        start: usize,
        full: bool,
    ) -> Option<Vec<Option<usize>>> {
        let code = &self.programs[program];
        let mut slots: Vec<Option<usize>> = vec![None; (self.groups + 1) * 2];
        let mut stack = vec![Frame::Try(0, start)];
        while let Some(frame) = stack.pop() {
            let (mut pc, mut at) = match frame {
                Frame::Try(pc, at) => (pc, at),
                Frame::Restore(slot, value) => {
                    slots[slot] = value;
                    continue;
                }
            };
            loop {
                match &code[pc] {
                    Inst::Class(index) => {
                        if at < text.len() && self.classes[*index].matches(text[at]) {
                            at += 1;
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Split(first, second) => {
                        stack.push(Frame::Try(*second, at));
                        pc = *first;
                    }
                    Inst::Jump(target) => pc = *target,
                    Inst::Save(slot) => {
                        stack.push(Frame::Restore(*slot, slots[*slot]));
                        slots[*slot] = Some(at);
                        pc += 1;
                    }
                    Inst::LookAhead(sub) => {
                        if self.run(*sub, text, at, false).is_some() {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Start => {
                        if at == 0 {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::End => {
                        if at == text.len() || (at + 1 == text.len() && text[at] == '\n') {
                            pc += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::WordBoundary => {
                        let before = at > 0 && is_word(text[at - 1]);
                        let after = at < text.len() && is_word(text[at]);
                        if before == after {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Match => {
                        if full && at != text.len() {
                            break;
                        }
                        if program != 0 {
                            return Some(Vec::new());
                        }
                        return Some(slots);
                    }
                }
            }
        }
        None
    }

    fn captures(slots: &[Option<usize>]) -> Captures {
        Captures {
            spans: slots
                .chunks(2)
                .map(|pair| match (pair[0], pair[1]) {
                    (Some(start), Some(end)) => Some((start, end)),
                    _ => None,
                })
                .collect(),
        }
    }

    /// `re.match(pattern, text, pos)`: a match starting exactly at `start`.
    pub(super) fn match_at(&self, text: &[char], start: usize) -> Option<Captures> {
        self.run(0, text, start, false)
            .map(|slots| Self::captures(&slots))
    }

    /// `re.fullmatch`: the whole text.
    pub(super) fn full_match(&self, text: &[char]) -> Option<Captures> {
        self.run(0, text, 0, true)
            .map(|slots| Self::captures(&slots))
    }

    /// Whether `text` is a full match.
    pub(super) fn is_full_match(&self, text: &[char]) -> bool {
        self.full_match(text).is_some()
    }

    /// `re.search` from `start`.
    pub(super) fn search_from(&self, text: &[char], start: usize) -> Option<Captures> {
        (start..=text.len()).find_map(|at| self.match_at(text, at))
    }

    /// Whether `re.search` finds a match.
    pub(super) fn is_found(&self, text: &[char]) -> bool {
        self.search_from(text, 0).is_some()
    }

    /// `re.finditer`: non-overlapping matches, left to right.
    pub(super) fn find_all(&self, text: &[char]) -> Vec<Captures> {
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

    /// `re.split` with a pattern holding one capturing group around the whole
    /// separator: pieces and separators alternate (`[piece, sep, piece, ...]`).
    pub(super) fn split_keeping(&self, text: &[char]) -> Vec<Vec<char>> {
        let mut parts = Vec::new();
        let mut cursor = 0;
        for captures in self.find_all(text) {
            parts.push(text[cursor..captures.start()].to_vec());
            parts.push(text[captures.start()..captures.end()].to_vec());
            cursor = captures.end();
        }
        parts.push(text[cursor..].to_vec());
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn ordered_alternation_and_backtracking_follow_python() {
        let value = Pattern::new(r#"(?:"[^"]*"|[^-\s;&|][^\s;&|]*)\s+x"#);
        // The quoted alternative fails at `\s+`, the run alternative takes over.
        assert_eq!(
            value.match_at(&chars(r#""a b"c x"#), 0).map(|c| c.end()),
            None
        );
        assert_eq!(
            value.match_at(&chars(r#""a b" x"#), 0).map(|c| c.end()),
            Some(7)
        );
        let full = Pattern::new(r"([A-Za-z_][A-Za-z0-9_]*)=(?:'([^']*)'|([A-Za-z0-9_./-]+))");
        let text = chars("G='git reset'");
        let captures = full.full_match(&text).expect("full match");
        assert_eq!(captures.text(&text, 1).as_deref(), Some("G"));
        assert_eq!(captures.text(&text, 2).as_deref(), Some("git reset"));
        assert_eq!(captures.group(3), None);
    }

    #[test]
    fn anchors_lookahead_and_word_boundaries() {
        let pattern = Pattern::new(r"\bgit(?=\s|$|[;&|)])");
        assert_eq!(
            pattern
                .find_all(&chars("xgit git; git\n"))
                .iter()
                .map(Captures::start)
                .collect::<Vec<_>>(),
            vec![5, 10]
        );
        let config = Pattern::new(r"core\.(worktree|bare)(=|$)");
        assert!(config.match_at(&chars("core.bare"), 0).is_some());
        assert!(config.match_at(&chars("core.bareX"), 0).is_none());
        let split = Pattern::new(r"(&&|\|\||;|\||\n)");
        assert_eq!(
            split.split_keeping(&chars("a&&b|c")),
            vec![chars("a"), chars("&&"), chars("b"), chars("|"), chars("c")]
        );
    }
}
