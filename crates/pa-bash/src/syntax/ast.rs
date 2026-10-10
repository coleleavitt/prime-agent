//! The parsed shape of a shell script: lists of pipelines of commands, each
//! simple command a sequence of words, each word a sequence of literal and
//! expansion parts with their quoting kept.
//!
//! The tree is what bash would execute, not a lossless rendering: comments,
//! line continuations and quote characters are gone, and a here-document
//! carries its body. Where the parser could not read the text (unbalanced
//! input, nesting past the depth bound), the unread text survives as an
//! [`Command::Unparsed`] node so a check can still look at it.

/// A sequence of and-or lists (`a; b & c`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct List {
    pub items: Vec<AndOr>,
}

/// Pipelines joined by `&&` / `||`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(Connector, Pipeline)>,
    /// Ended by `&`: runs in the background.
    pub background: bool,
}

impl AndOr {
    pub(crate) fn pipelines(&self) -> impl Iterator<Item = &Pipeline> {
        std::iter::once(&self.first).chain(self.rest.iter().map(|(_, pipeline)| pipeline))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connector {
    And,
    Or,
}

/// Commands joined by `|` / `|&`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Pipeline {
    pub negated: bool,
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    Simple(SimpleCommand),
    /// `( list )`.
    Subshell(List, Vec<Redirect>),
    /// `{ list; }`.
    Group(List, Vec<Redirect>),
    /// `if`/`while`/`until` (and `elif`/`else`): every condition and body.
    Branches(Vec<List>, Vec<Redirect>),
    /// `for NAME in WORDS; do BODY; done` (`words` is `None` for the
    /// positional-parameter form) and `select`.
    For {
        name: String,
        words: Option<Vec<Word>>,
        body: List,
        redirects: Vec<Redirect>,
    },
    /// `case WORD in PATTERNS) BODY;; esac`: the patterns are words, never
    /// commands.
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, List)>,
        redirects: Vec<Redirect>,
    },
    /// `[[ expression ]]`: its words (substitutions inside them run).
    Conditional(Vec<Word>),
    /// `(( expression ))` and arithmetic `for (( ;; ))` headers: the raw
    /// expression and any substitutions inside it.
    Arithmetic(Word),
    /// `name() BODY` / `function name BODY`.
    Function {
        name: String,
        body: Box<Command>,
    },
    /// Text the parser could not read (unbalanced, or nested past the
    /// bound).
    Unparsed(String),
}

/// `NAME=value` or `NAME=(values)` before a command word (or alone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub name: String,
    pub value: AssignValue,
    /// `+=`.
    pub append: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AssignValue {
    Scalar(Word),
    Array(Vec<Word>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SimpleCommand {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedirectOp {
    /// `<`
    Input,
    /// `>`, `>|`, `&>`
    Output,
    /// `>>`, `&>>`
    Append,
    /// `<>`
    ReadWrite,
    /// `<&`, `>&` (duplicate a descriptor, or `>& file`)
    Duplicate,
    /// `<<`, `<<-`
    HereDoc,
    /// `<<<`
    HereString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// The explicit descriptor (`2>`), when one was written.
    pub fd: Option<u32>,
    pub op: RedirectOp,
    /// The file, descriptor or here-string word; a here-document's
    /// delimiter.
    pub target: Word,
    /// A here-document's body.
    pub heredoc: Option<HereDoc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HereDoc {
    /// The body as written (without the delimiter line).
    pub raw: String,
    /// The body as the reading command sees it: expansions are parsed when
    /// the delimiter was unquoted, literal otherwise.
    pub body: Word,
    /// While parsing: the body's slot among the bodies still to be read.
    pub(super) slot: Option<usize>,
}

/// One shell word.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Word {
    pub parts: Vec<Part>,
    /// The source text of the word.
    pub raw: String,
}

/// How a literal part was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quoting {
    /// Unquoted: globs, braces and a leading tilde are live.
    Bare,
    /// `'...'`, `$'...'`, a backslash escape.
    Single,
    /// `"..."` (and `$"..."`).
    Double,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Part {
    Literal(String, Quoting),
    /// `$name`, `${name...}`, `$1`, `$@`, ...: `name` is the parameter,
    /// `operator` the text after it inside braces (`:-default`, `#*/`),
    /// parsed for the substitutions it runs.
    Parameter {
        name: String,
        operator: Option<Box<Word>>,
        quoted: bool,
    },
    /// `$(...)` or a backquoted command.
    Command {
        body: List,
        quoted: bool,
    },
    /// `$((...))`.
    Arithmetic(Box<Word>),
    /// `<(...)` / `>(...)`.
    Process {
        body: List,
        input: bool,
    },
}

impl Part {
    pub(crate) fn is_expansion(&self) -> bool {
        !matches!(self, Part::Literal(..))
    }
}

impl Word {
    pub(crate) fn literal(text: &str) -> Self {
        Self {
            parts: vec![Part::Literal(text.to_string(), Quoting::Bare)],
            raw: text.to_string(),
        }
    }

    /// The word after quote removal when it holds no expansion and no live
    /// glob or brace character: what the command receives, verbatim.
    pub(crate) fn as_static(&self) -> Option<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(text, Quoting::Bare) => {
                    if text.contains(['*', '?', '[', '{']) {
                        return None;
                    }
                    out.push_str(text);
                }
                Part::Literal(text, Quoting::Single | Quoting::Double) => out.push_str(text),
                Part::Parameter { .. }
                | Part::Command { .. }
                | Part::Arithmetic(_)
                | Part::Process { .. } => return None,
            }
        }
        Some(out)
    }

    pub(crate) fn has_expansion(&self) -> bool {
        self.parts.iter().any(Part::is_expansion)
    }

    /// The word's text with quoting removed and every expansion kept as
    /// written: the evidence a check reads when it cannot evaluate the word.
    pub(crate) fn visible(&self) -> String {
        let mut out = String::new();
        self.push_visible(&mut out);
        out
    }

    fn push_visible(&self, out: &mut String) {
        if self.has_expansion() {
            out.push_str(&self.raw);
            for part in &self.parts {
                part.push_payload_text(out);
            }
        } else {
            for part in &self.parts {
                if let Part::Literal(text, _) = part {
                    out.push_str(text);
                }
            }
        }
    }
}

impl Part {
    /// Decoded text nested inside an expansion that its raw spelling hides
    /// (an ANSI-C string inside a substitution).
    fn push_payload_text(&self, out: &mut String) {
        match self {
            Part::Literal(..) | Part::Arithmetic(_) => {}
            Part::Parameter { operator, .. } => {
                if let Some(operator) = operator {
                    for part in &operator.parts {
                        part.push_payload_text(out);
                    }
                }
            }
            Part::Command { body, .. } | Part::Process { body, .. } => {
                out.push(' ');
                out.push_str(&body.visible());
            }
        }
    }
}

impl List {
    /// Every word of the list, decoded where it is literal: the text a
    /// check searches for evidence.
    pub(crate) fn visible(&self) -> String {
        let mut out = String::new();
        crate::syntax::walk::visit_words(self, &mut |word| {
            out.push_str(&word.visible());
            out.push(' ');
        });
        out
    }
}
