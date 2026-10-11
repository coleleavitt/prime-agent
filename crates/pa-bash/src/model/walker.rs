//! Building the model: one walk over the parsed script in execution order,
//! carrying what the text fixes (variables, positional parameters, the
//! directories commands run in, functions, aliases, `hash -p` entries) and
//! descending into nested code. The commands themselves are dispatched in
//! [`super::commands`].

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use super::value::{self, Arg, Lookup, Piece, Resolve, Unknown, Var, Vars};
use super::{
    Capture,
    Cwd,
    Input,
    Invocation,
    Layer,
    Model,
    Opaque,
    OpaqueKind,
    Output,
    Stage,
    Via,
};
use crate::context::GuardContext;
use crate::syntax::ast::{
    AndOr,
    AssignValue,
    Assignment,
    Command,
    Connector,
    List,
    Part,
    Pipeline,
    Redirect,
    RedirectOp,
    SimpleCommand,
    Word,
};
use crate::syntax::parse::{NEW_DESCRIPTOR, parse};

/// Nested code (payloads, scripts, here-documents fed to shells, function
/// calls) deeper than this stays opaque.
pub(super) const MAX_CODE_DEPTH: usize = 12;
/// How often one function's body is walked at its call sites before further
/// calls are judged on its visible text: a call tree that fans out costs a
/// walk per path.
pub(super) const MAX_FUNCTION_WALKS: usize = 16;
/// Invocations past this are not recorded; the rest of the text is judged
/// as one opaque node.
pub(super) const MAX_INVOCATIONS: usize = 20_000;

/// The script's model.
pub(super) fn build(text: &str, context: &GuardContext) -> Model {
    let workspace = context
        .cwd()
        .canonicalize()
        .unwrap_or_else(|_| context.cwd().to_path_buf());
    let mut walker = Walker {
        workspace,
        model: Model::default(),
        via: Vec::new(),
        depth: 0,
        budget: (16 * text.len()).max(4 * 1024 * 1024),
        stdin: vec![Feed::Kernel],
        stdout: vec![Output::Transcript],
        stage: None,
        stage_depth: 0,
        stage_last: true,
        pipelines: 0,
        fragments: HashMap::new(),
        written: HashMap::new(),
        exhausted: false,
        errexit: false,
        expanding: Vec::new(),
        function_walks: HashMap::new(),
        unwalked: std::collections::HashSet::new(),
    };
    let mut state = State {
        vars: Vars::new(context.env()),
        cwd: Cwd::at(context.cwd().to_path_buf()),
        positional: Vec::new(),
        functions: HashMap::new(),
        aliases: HashMap::new(),
        expand_aliases: false,
        hashed: HashMap::new(),
    };
    let parsed = parse(text);
    // Text after a syntax error never runs (bash rejects its whole line):
    // it is not modelled.
    walker.list(&parsed.list, &mut state);
    let trailing = text.len() - text.trim_end_matches('\\').len();
    if trailing % 2 == 1 {
        // A final line continuation joins the command with whatever the
        // kernel appends after it.
        let last = text.lines().last().unwrap_or(text).to_string();
        walker.model.opaques.push(Opaque {
            kind: OpaqueKind::Unparsed,
            shown: format!(
                "{} (it ends with a line continuation)",
                super::shorten(&last, 80)
            ),
            evidence: last,
            context: Vec::new(),
        });
    }
    if walker.exhausted {
        walker.model.opaques.push(Opaque {
            kind: OpaqueKind::Unparsed,
            shown: "the rest of this command (too large to model)".to_string(),
            evidence: text.to_string(),
            context: Vec::new(),
        });
    }
    walker.model
}

#[derive(Debug, Clone)]
pub(super) struct State<'e> {
    pub vars: Vars<'e>,
    pub cwd: Cwd,
    /// `$1`, `$2`, ... (`$@`).
    pub positional: Vec<Arg>,
    pub functions: HashMap<String, Rc<Command>>,
    pub aliases: HashMap<String, String>,
    /// `shopt -s expand_aliases` is known to be on. Off (bash's default for
    /// a script) or unknown, a command word is judged both as the alias
    /// body and as itself.
    pub expand_aliases: bool,
    /// `hash -p PATH NAME` entries.
    pub hashed: HashMap<String, Arg>,
}

impl State<'_> {
    /// `$HOME` as the script sees it now.
    pub(super) fn home(&self) -> Option<String> {
        match self.vars.get("HOME") {
            Lookup::Value(home) if !home.is_empty() => Some(home),
            Lookup::Value(_) | Lookup::Unknown(_) | Lookup::Unset => None,
        }
    }
}

/// What a command's stdin carries.
#[derive(Debug, Clone)]
pub(super) enum Feed {
    /// The kernel's stdin: nothing.
    Kernel,
    /// The previous pipeline stage.
    Pipe,
    /// Text (a here-document, a here-string, echoed text) with unknown
    /// parts spliced in as placeholders.
    Code(String),
    /// `< file`.
    File(Arg),
    /// A stream the text does not show: its visible source.
    Hidden(String),
    /// Pipeline output the model cannot reconstruct: the visible text of
    /// the commands that write it (each one in the model too).
    Piped(String),
}

/// One argv field and the word it came from.
#[derive(Debug, Clone)]
pub(super) struct Field<'w> {
    pub arg: Arg,
    pub word: Option<&'w Word>,
    /// The word produced exactly this one field.
    pub whole: bool,
    /// The field's text as shell code, when it differs from the word's
    /// (an `xargs -I` token spliced in as a placeholder).
    pub code: Option<String>,
}

impl Field<'_> {
    pub(super) fn of(arg: Arg) -> Self {
        Self {
            arg,
            word: None,
            whole: true,
            code: None,
        }
    }
}

pub(super) struct Walker {
    pub workspace: PathBuf,
    pub model: Model,
    pub via: Vec<Via>,
    pub depth: usize,
    pub budget: usize,
    pub stdin: Vec<Feed>,
    pub stdout: Vec<Output>,
    pub stage: Option<usize>,
    pub stage_depth: usize,
    pub stage_last: bool,
    pub pipelines: usize,
    pub fragments: HashMap<String, String>,
    /// Files this command writes before reading them back (`cat > x.sh
    /// <<EOF`), with their text.
    pub written: HashMap<PathBuf, String>,
    pub exhausted: bool,
    pub errexit: bool,
    /// Aliases being expanded (bash never re-expands one inside itself).
    pub expanding: Vec<String>,
    /// Walks of each function's body so far.
    pub function_walks: HashMap<String, usize>,
    /// Functions already judged on their text past the walk bounds.
    pub unwalked: std::collections::HashSet<String>,
}

/// Expansion results of one command's substitutions, by part address.
pub(super) type Pieces = HashMap<usize, (Piece, Range<usize>)>;

fn address(part: &Part) -> usize {
    std::ptr::from_ref(part) as usize
}

/// Everything about one simple command besides its argv.
pub(super) struct Call {
    pub env: Vec<(String, Arg)>,
    pub feed: Option<Feed>,
    pub output: Option<Output>,
    /// The substitution invocations of each word, by word address.
    pub word_subs: HashMap<usize, Range<usize>>,
    /// The substitution invocations of the redirects (targets, here-document
    /// bodies).
    pub redirect_subs: Vec<Range<usize>>,
    pub pieces: Pieces,
}

impl Call {
    /// The same command context for another run of the command (`xargs`
    /// running it once per input line).
    pub(super) fn again(&self) -> Self {
        Self {
            env: self.env.clone(),
            feed: None,
            output: self.output.clone(),
            word_subs: self.word_subs.clone(),
            redirect_subs: Vec::new(),
            pieces: self.pieces.clone(),
        }
    }

    pub(super) fn bare() -> Self {
        Self {
            env: Vec::new(),
            feed: None,
            output: None,
            word_subs: HashMap::new(),
            redirect_subs: Vec::new(),
            pieces: Pieces::new(),
        }
    }

    fn all_subs(&self) -> Vec<Range<usize>> {
        self.word_subs
            .values()
            .chain(&self.redirect_subs)
            .filter(|range| !range.is_empty())
            .cloned()
            .collect()
    }
}

pub(super) struct Resolver<'a, 'e> {
    state: &'a State<'e>,
    pieces: &'a Pieces,
    fragments: &'a HashMap<String, String>,
    home: Option<String>,
}

impl Resolver<'_, '_> {
    fn lookup(&self, name: &str) -> Lookup {
        if name == "PWD" && !self.state.vars.set_by_script("PWD") {
            if let ([dir], None) = (self.state.cwd.dirs.as_slice(), &self.state.cwd.unknown) {
                return Lookup::Value(dir.display().to_string());
            }
        }
        if name == "0" {
            return Lookup::Value("bash".to_string());
        }
        if let Ok(index) = name.parse::<usize>() {
            return match self.state.positional.get(index.wrapping_sub(1)) {
                Some(Arg::Known(text)) => Lookup::Value(text.clone()),
                Some(other) => Lookup::Unknown(other.evidence()),
                None => Lookup::Unset,
            };
        }
        if name == "#" {
            return Lookup::Value(self.state.positional.len().to_string());
        }
        self.state.vars.get(name)
    }
}

impl Resolve for Resolver<'_, '_> {
    fn parameter(&self, name: &str, operator: Option<&Word>) -> Piece {
        if let Some(evidence) = self.fragments.get(name) {
            return Piece::Unknown(evidence.clone());
        }
        if matches!(name, "@" | "*") && operator.is_none() {
            let known: Option<Vec<String>> = self
                .state
                .positional
                .iter()
                .map(|arg| arg.known().map(str::to_string))
                .collect();
            return match known {
                Some(texts) if name == "@" => Piece::Fields(texts),
                Some(texts) => Piece::Text(texts.join(" ")),
                None => Piece::Unknown(
                    self.state
                        .positional
                        .iter()
                        .map(Arg::evidence)
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            };
        }
        let lookup = self.lookup(name);
        let current = match &lookup {
            Lookup::Value(value) => Piece::Text(value.clone()),
            Lookup::Unknown(source) => Piece::Unknown(source.clone()),
            Lookup::Unset => Piece::Text(String::new()),
        };
        let Some(operator) = operator else {
            return current;
        };
        let raw = operator.raw.as_str();
        let Some(skip) = [":-", "-", ":=", "="]
            .iter()
            .find(|prefix| raw.starts_with(**prefix))
            .map(|prefix| prefix.len())
        else {
            // `${x#pat}`, `${x/a/b}`, `${#x}`, ...: derived from the value.
            return Piece::Unknown(format!("{name} {}", operator.visible()));
        };
        let empty = match &lookup {
            Lookup::Unset => Some(true),
            Lookup::Value(value) => Some(value.is_empty() && raw.starts_with(':')),
            Lookup::Unknown(_) => None,
        };
        let mut tail = operator.clone();
        strip_prefix_chars(&mut tail, skip);
        let fallback = match value::single(&tail, self, self.home.as_deref()) {
            Arg::Known(text) => Piece::Text(text),
            other => Piece::Unknown(other.evidence()),
        };
        match (empty, current, fallback) {
            (Some(true), _, fallback) => fallback,
            (None, Piece::Unknown(source), Piece::Text(text) | Piece::Unknown(text)) => {
                Piece::Unknown(format!("{source} {text}"))
            }
            (Some(false) | None, current, _) => current,
        }
    }

    fn command(&self, part: &Part) -> Piece {
        self.pieces
            .get(&address(part))
            .map_or_else(|| Piece::Unknown(String::new()), |(piece, _)| piece.clone())
    }
}

/// Drop the first `count` chars of a word's literal start (the `:-` of a
/// default operator).
fn strip_prefix_chars(word: &mut Word, mut count: usize) {
    while count > 0 {
        let Some(Part::Literal(text, _)) = word.parts.first_mut() else {
            return;
        };
        let taken = text.chars().take(count).count();
        let bytes: usize = text.chars().take(count).map(char::len_utf8).sum();
        text.replace_range(..bytes, "");
        count -= taken;
        if text.is_empty() {
            word.parts.remove(0);
        }
        if taken == 0 {
            return;
        }
    }
}

impl Walker {
    pub(super) fn resolver<'a, 'e>(
        &'a self,
        state: &'a State<'e>,
        pieces: &'a Pieces,
    ) -> Resolver<'a, 'e> {
        Resolver {
            state,
            pieces,
            fragments: &self.fragments,
            home: state.home(),
        }
    }

    // ---- lists ----------------------------------------------------------

    pub(super) fn list(&mut self, list: &List, state: &mut State<'_>) {
        for item in &list.items {
            if item.background {
                // `cmd &` runs in a subshell: nothing it sets reaches the
                // commands after it.
                let mut inner = state.clone();
                self.and_or(item, &mut inner);
                continue;
            }
            let before = state.cwd.clone();
            self.and_or(item, state);
            if state.cwd != before && !self.errexit && !exits_on_failure(item) {
                // A `cd` that may have failed: later commands run in either
                // place.
                state.cwd.union(&before);
            }
        }
    }

    fn and_or(&mut self, item: &AndOr, state: &mut State<'_>) {
        let mut before_left = state.cwd.clone();
        self.pipeline(&item.first, state);
        for (connector, pipeline) in &item.rest {
            let after_left = state.cwd.clone();
            if *connector == Connector::Or {
                state.cwd = before_left.clone();
            }
            before_left = state.cwd.clone();
            self.pipeline(pipeline, state);
            if *connector == Connector::Or {
                state.cwd.union(&after_left);
            }
        }
    }

    fn pipeline(&mut self, pipeline: &Pipeline, state: &mut State<'_>) {
        let before = state.cwd.clone();
        if pipeline.commands.len() == 1 {
            self.command(&pipeline.commands[0], state);
        } else {
            let id = self.pipelines;
            self.pipelines += 1;
            let count = pipeline.commands.len();
            for (index, command) in pipeline.commands.iter().enumerate() {
                let stage_index = self.model.stages.len();
                let start = self.model.invocations.len();
                self.model.stages.push(Stage {
                    pipeline: id,
                    index,
                    range: start..start,
                    parent: self.stage,
                });
                let saved = (self.stage, self.stage_depth, self.stage_last);
                self.stage = Some(stage_index);
                self.stage_depth = 0;
                self.stage_last = index + 1 == count;
                if index > 0 {
                    self.stdin.push(Feed::Pipe);
                }
                if !self.stage_last {
                    self.stdout.push(Output::Pipe);
                }
                let mut child = state.clone();
                self.command(command, &mut child);
                if !self.stage_last {
                    self.stdout.pop();
                }
                if index > 0 {
                    self.stdin.pop();
                }
                (self.stage, self.stage_depth, self.stage_last) = saved;
                self.model.stages[stage_index].range.end = self.model.invocations.len();
            }
        }
        if pipeline.negated {
            // `! cd x && y`: y runs where the cd failed.
            state.cwd.union(&before);
        }
    }

    fn command(&mut self, command: &Command, state: &mut State<'_>) {
        match command {
            Command::Simple(simple) => self.simple(simple, state),
            Command::Subshell(list, redirects) => {
                let pushed = self.compound_redirects(redirects, state);
                let mut child = state.clone();
                self.list(list, &mut child);
                self.pop_redirects(pushed);
            }
            Command::Group(list, redirects) => {
                let pushed = self.compound_redirects(redirects, state);
                self.list(list, state);
                self.pop_redirects(pushed);
            }
            Command::Branches(lists, redirects) => {
                let pushed = self.compound_redirects(redirects, state);
                let before = state.clone();
                for list in lists {
                    self.list(list, state);
                }
                merge(state, &before);
                self.pop_redirects(pushed);
            }
            Command::For {
                name,
                words,
                body,
                redirects,
            } => {
                let pushed = self.compound_redirects(redirects, state);
                let mut source = String::new();
                for word in words.iter().flatten() {
                    self.walk_substitutions(word, state, &mut Pieces::new(), None);
                    source.push_str(&word.visible());
                    source.push(' ');
                }
                let before = state.clone();
                state.vars.assign(
                    name,
                    Var {
                        value: None,
                        source,
                    },
                );
                self.list(body, state);
                merge(state, &before);
                self.pop_redirects(pushed);
            }
            Command::Case {
                word,
                arms,
                redirects,
            } => {
                let pushed = self.compound_redirects(redirects, state);
                self.walk_substitutions(word, state, &mut Pieces::new(), None);
                let before = state.clone();
                let mut after = state.clone();
                for (_, body) in arms {
                    let mut arm = before.clone();
                    self.list(body, &mut arm);
                    merge(&mut after, &arm);
                }
                *state = after;
                self.pop_redirects(pushed);
            }
            Command::Conditional(words) => {
                for word in words {
                    self.walk_substitutions(word, state, &mut Pieces::new(), None);
                }
            }
            Command::Arithmetic(word) => {
                self.walk_substitutions(word, state, &mut Pieces::new(), None);
            }
            Command::Function { name, body } => {
                // A definition runs nothing: the body is judged where it is
                // called.
                state
                    .functions
                    .insert(name.clone(), Rc::new((**body).clone()));
            }
            Command::Unparsed(text) => {
                self.model.opaques.push(Opaque {
                    kind: OpaqueKind::Unparsed,
                    shown: text.clone(),
                    evidence: self.with_fragments(text),
                    context: self.via.clone(),
                });
            }
        }
    }

    /// Push the stdin/stdout a compound command's redirects give its body.
    fn compound_redirects(
        &mut self,
        redirects: &[Redirect],
        state: &mut State<'_>,
    ) -> (bool, bool) {
        let mut pieces = Pieces::new();
        for redirect in redirects {
            self.walk_substitutions(&redirect.target, state, &mut pieces, None);
            if let Some(heredoc) = &redirect.heredoc {
                self.walk_substitutions(&heredoc.body, state, &mut pieces, None);
            }
        }
        let (feed, output) = self.redirect_io(redirects, state, &pieces);
        let pushed_in = feed.is_some();
        if let Some(feed) = feed {
            self.stdin.push(feed);
        }
        let pushed_out = output.is_some();
        if let Some(output) = output {
            self.stdout.push(output);
        }
        (pushed_in, pushed_out)
    }

    fn pop_redirects(&mut self, (pushed_in, pushed_out): (bool, bool)) {
        if pushed_in {
            self.stdin.pop();
        }
        if pushed_out {
            self.stdout.pop();
        }
    }

    // ---- simple commands ------------------------------------------------

    /// Walk the commands inside `word`'s substitutions (they run before the
    /// command does) and note what each expands to. `writer` names the
    /// command whose output a `>(...)` reads. Returns the range of
    /// invocations they recorded.
    fn walk_substitutions(
        &mut self,
        word: &Word,
        state: &State<'_>,
        pieces: &mut Pieces,
        writer: Option<&str>,
    ) -> Range<usize> {
        let start = self.model.invocations.len();
        for part in &word.parts {
            match part {
                Part::Literal(..) => {}
                Part::Parameter { operator, .. } => {
                    if let Some(operator) = operator {
                        self.walk_substitutions(operator, state, pieces, writer);
                    }
                }
                Part::Arithmetic(expression) => {
                    self.walk_substitutions(expression, state, pieces, writer);
                }
                Part::Command { body, .. } | Part::Process { body, .. } => {
                    let from = self.model.invocations.len();
                    let mut child = state.clone();
                    self.via.push(Via::Substitution);
                    self.stage_depth += 1;
                    let reads_output = matches!(part, Part::Process { input: false, .. });
                    self.stdout.push(if reads_output {
                        Output::Transcript
                    } else {
                        Output::Captured
                    });
                    if reads_output {
                        self.stdin.push(Feed::Hidden(format!(
                            "the output of {}",
                            writer.unwrap_or("the command")
                        )));
                    }
                    self.list(body, &mut child);
                    if reads_output {
                        self.stdin.pop();
                    }
                    self.stdout.pop();
                    self.stage_depth -= 1;
                    self.via.pop();
                    let to = self.model.invocations.len();
                    let piece = match command_output(body, &child) {
                        Some(text) => Piece::Text(text),
                        None => Piece::Unknown(body.visible()),
                    };
                    pieces.insert(address(part), (piece, from..to));
                }
            }
        }
        start..self.model.invocations.len()
    }

    fn simple(&mut self, command: &SimpleCommand, state: &mut State<'_>) {
        let mut pieces = Pieces::new();
        let writer: String = command
            .words
            .iter()
            .map(|word| word.raw.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        for assignment in &command.assignments {
            match &assignment.value {
                AssignValue::Scalar(word) => {
                    self.walk_substitutions(word, state, &mut pieces, None);
                }
                AssignValue::Array(words) => {
                    for word in words {
                        self.walk_substitutions(word, state, &mut pieces, None);
                    }
                }
            }
        }
        let mut word_subs = HashMap::new();
        for word in &command.words {
            let range = self.walk_substitutions(word, state, &mut pieces, Some(&writer));
            word_subs.insert(std::ptr::from_ref(word) as usize, range);
        }
        let mut redirect_subs = Vec::new();
        for redirect in &command.redirects {
            redirect_subs.push(self.walk_substitutions(
                &redirect.target,
                state,
                &mut pieces,
                Some(&writer),
            ));
            if let Some(heredoc) = &redirect.heredoc {
                redirect_subs.push(self.walk_substitutions(
                    &heredoc.body,
                    state,
                    &mut pieces,
                    None,
                ));
            }
        }
        let mut env = Vec::new();
        for assignment in &command.assignments {
            let (value, var) = self.assignment(assignment, state, &pieces);
            if command.words.is_empty() {
                state.vars.assign(&assignment.name, var);
            } else {
                env.push((assignment.name.clone(), value));
            }
        }
        let (feed, output) = self.redirect_io(&command.redirects, state, &pieces);
        if command.words.is_empty() {
            return;
        }
        let resolver = self.resolver(state, &pieces);
        let home = state.home();
        let mut fields: Vec<Field<'_>> = Vec::new();
        for word in &command.words {
            let produced = value::fields(word, &resolver, home.as_deref());
            let whole = produced.len() == 1;
            for arg in produced {
                fields.push(Field {
                    arg,
                    word: Some(word),
                    whole,
                    code: None,
                });
            }
        }
        redirect_subs.retain(|range| !range.is_empty());
        let call = Call {
            env,
            feed,
            output,
            word_subs,
            redirect_subs,
            pieces,
        };
        self.run(fields, Vec::new(), call, state);
    }

    /// An assignment's value as an argument, and the variable it sets.
    fn assignment(
        &self,
        assignment: &Assignment,
        state: &State<'_>,
        pieces: &Pieces,
    ) -> (Arg, Var) {
        let resolver = self.resolver(state, pieces);
        let home = state.home();
        let (value, source) = match &assignment.value {
            AssignValue::Scalar(word) => (
                value::single(word, &resolver, home.as_deref()),
                word.visible(),
            ),
            AssignValue::Array(words) => {
                let source: Vec<String> = words.iter().map(Word::visible).collect();
                (
                    Arg::Unknown(Unknown {
                        prefix: String::new(),
                        written: format!("({})", source.join(" ")),
                        source: source.join(" "),
                        input: false,
                    }),
                    source.join(" "),
                )
            }
        };
        let fixed = (!assignment.append)
            .then(|| value.known().map(str::to_string))
            .flatten();
        let var = Var {
            value: fixed,
            source: format!("{} {}", source, value.evidence()),
        };
        (value, var)
    }

    /// The stdin feed and stdout target `redirects` set.
    fn redirect_io(
        &mut self,
        redirects: &[Redirect],
        state: &State<'_>,
        pieces: &Pieces,
    ) -> (Option<Feed>, Option<Output>) {
        let mut feed = None;
        let mut output = None;
        let home = state.home();
        for redirect in redirects {
            if redirect.fd == Some(NEW_DESCRIPTOR) {
                continue;
            }
            let resolver = self.resolver(state, pieces);
            match redirect.op {
                RedirectOp::HereDoc if redirect.fd.is_none_or(|fd| fd == 0) => {
                    if let Some(heredoc) = &redirect.heredoc {
                        feed = Some(Feed::Code(self.code_text(&heredoc.body, state, pieces)));
                    }
                }
                RedirectOp::HereString if redirect.fd.is_none_or(|fd| fd == 0) => {
                    let mut text = self.code_text(&redirect.target, state, pieces);
                    text.push('\n');
                    feed = Some(Feed::Code(text));
                }
                RedirectOp::Input | RedirectOp::ReadWrite
                    if redirect.fd.is_none_or(|fd| fd == 0) =>
                {
                    feed = Some(match redirect.target.parts.as_slice() {
                        [Part::Process { body, .. }] => Feed::Hidden(body.visible()),
                        // The shell opens the file here, before any payload
                        // runs elsewhere: fix it against this directory.
                        _ => Feed::File(
                            match value::single(&redirect.target, &resolver, home.as_deref()) {
                                Arg::Known(path) => match state.cwd.dirs.first() {
                                    Some(dir) if !path.starts_with('/') => Arg::Known(
                                        super::files::join(dir, &path).display().to_string(),
                                    ),
                                    Some(_) | None => Arg::Known(path),
                                },
                                other => other,
                            },
                        ),
                    });
                }
                RedirectOp::Output | RedirectOp::Append | RedirectOp::ReadWrite
                    if redirect.fd.is_none_or(|fd| fd == 1) || redirect.fd == Some(1) =>
                {
                    if redirect.op == RedirectOp::ReadWrite && redirect.fd != Some(1) {
                        continue;
                    }
                    let target = value::single(&redirect.target, &resolver, home.as_deref());
                    output = Some(file_output(target));
                }
                RedirectOp::Duplicate if redirect.fd.is_none_or(|fd| fd == 1) => {
                    output = match redirect.target.as_static() {
                        Some(target)
                            if target.chars().all(|ch| ch.is_ascii_digit() || ch == '-')
                                && target != "2" =>
                        {
                            output
                        }
                        Some(target) if target != "2" => Some(file_output(Arg::Known(target))),
                        // `>&2`, or a descriptor only known at run time.
                        Some(_) | None => Some(Output::Transcript),
                    };
                }
                RedirectOp::HereDoc
                | RedirectOp::HereString
                | RedirectOp::Input
                | RedirectOp::ReadWrite
                | RedirectOp::Output
                | RedirectOp::Append
                | RedirectOp::Duplicate => {}
            }
        }
        (feed, output)
    }

    /// The text `word` gives a shell as code: literal parts as written,
    /// fixed expansions as their value, unknown ones as placeholder
    /// parameters (each an opaque fragment carrying its source text).
    pub(super) fn code_text(&mut self, word: &Word, state: &State<'_>, pieces: &Pieces) -> String {
        let mut out = String::new();
        for part in &word.parts {
            let piece = match part {
                Part::Literal(text, _) => {
                    out.push_str(text);
                    continue;
                }
                Part::Parameter { name, operator, .. } => self
                    .resolver(state, pieces)
                    .parameter(name, operator.as_deref()),
                Part::Command { .. } | Part::Process { .. } => {
                    self.resolver(state, pieces).command(part)
                }
                Part::Arithmetic(expression) => Piece::Unknown(expression.visible()),
            };
            match piece {
                Piece::Text(text) => out.push_str(&text),
                Piece::Fields(texts) => out.push_str(&texts.join(" ")),
                Piece::Unknown(evidence) => out.push_str(&self.placeholder(evidence)),
            }
        }
        out
    }

    pub(super) fn placeholder(&mut self, evidence: String) -> String {
        let name = format!("__PA_FRAGMENT_{}__", self.fragments.len());
        let shown = format!("${{{name}}}");
        self.fragments.insert(name, evidence);
        shown
    }

    pub(super) fn current_feed(&self, call: &Call) -> Feed {
        let feed = call
            .feed
            .clone()
            .unwrap_or_else(|| self.stdin.last().cloned().unwrap_or(Feed::Kernel));
        match feed {
            Feed::Pipe => self.pipe_feed(),
            other => other,
        }
    }

    /// The code a pipe-fed shell reads: what the earlier stages print when
    /// they only print fixed text, else hidden.
    fn pipe_feed(&self) -> Feed {
        let producers = self.current_producers();
        let mut text = String::new();
        for producer in &producers {
            match emitted_text(producer, &self.written) {
                Some(emitted) => text.push_str(&emitted),
                None => return Feed::Piped(self.producer_text()),
            }
        }
        if producers.is_empty() {
            Feed::Piped(String::new())
        } else {
            Feed::Code(text)
        }
    }

    fn current_producers(&self) -> Vec<Invocation> {
        let mut out = Vec::new();
        let mut stage = self.stage;
        while let Some(index) = stage {
            let current = &self.model.stages[index];
            for earlier in &self.model.stages {
                if earlier.pipeline == current.pipeline && earlier.index < current.index {
                    out.extend(
                        self.model.invocations[earlier.range.clone()]
                            .iter()
                            .cloned(),
                    );
                }
            }
            stage = current.parent;
        }
        out
    }

    /// The visible text of what feeds the current command's stdin: the
    /// earlier stages (and what they print), or a here-document.
    pub(super) fn producer_text(&self) -> String {
        let mut parts: Vec<String> = self
            .current_producers()
            .iter()
            .map(|producer| {
                let mut text: Vec<String> = producer.argv.iter().map(Arg::evidence).collect();
                if let Some(emitted) = emitted_text(producer, &self.written) {
                    text.push(emitted);
                }
                text.join(" ")
            })
            .collect();
        match self.stdin.last() {
            Some(Feed::Code(text) | Feed::Hidden(text) | Feed::Piped(text)) => {
                parts.push(text.clone());
            }
            Some(Feed::File(path)) => parts.push(path.evidence()),
            Some(Feed::Kernel | Feed::Pipe) | None => {}
        }
        parts.join(" | ")
    }

    // ---- nested code -----------------------------------------------------

    /// Walk `text` as shell code nested in the current command. `cwd`
    /// overrides where it runs; `feed` what its commands read.
    pub(super) fn code(
        &mut self,
        text: &str,
        via: Via,
        state: &mut State<'_>,
        share: Share,
        feed: Option<Feed>,
        cwd: Option<Cwd>,
    ) {
        if text.trim().is_empty() {
            return;
        }
        if self.depth >= MAX_CODE_DEPTH
            || text.len() > self.budget
            || self.model.invocations.len() >= MAX_INVOCATIONS
        {
            let mut context = self.via.clone();
            context.push(via);
            self.model.opaques.push(Opaque {
                kind: OpaqueKind::Unparsed,
                shown: super::shorten(&self.show_code(text), 80),
                evidence: self.with_fragments(text),
                context,
            });
            if share == Share::Shared {
                state.cwd.unknown = Some(super::shorten(&self.show_code(text), 60));
            }
            return;
        }
        self.budget -= text.len();
        let parsed = parse(text);
        self.depth += 1;
        self.via.push(via);
        self.stage_depth += 1;
        let pushed = feed.is_some();
        if let Some(feed) = feed {
            self.stdin.push(feed);
        }
        match share {
            Share::Shared => {
                if let Some(cwd) = cwd {
                    state.cwd = cwd;
                }
                self.list(&parsed.list, state);
            }
            Share::Copy => {
                let mut child = state.clone();
                if let Some(cwd) = cwd {
                    child.cwd = cwd;
                }
                self.list(&parsed.list, &mut child);
            }
        }
        if pushed {
            self.stdin.pop();
        }
        self.stage_depth -= 1;
        self.via.pop();
        self.depth -= 1;
        if share == Share::Shared && !self.fragments_in(text).is_empty() {
            // `eval "$x"` may change directory.
            state
                .cwd
                .unknown
                .get_or_insert(super::shorten(&self.show_code(text), 120));
        }
    }

    fn fragments_in(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("__PA_FRAGMENT_") {
            let tail = &rest[at..];
            let end = tail[14..].find("__").map_or(tail.len(), |end| end + 16);
            if let Some(evidence) = self.fragments.get(&tail[..end]) {
                out.push(evidence.clone());
            }
            rest = &tail[end.min(tail.len())..];
        }
        out
    }

    /// `text` with placeholders shown as `...`.
    #[expect(clippy::unused_self, reason = "the placeholders are the walker's own")]
    pub(super) fn show_code(&self, text: &str) -> String {
        super::hide_placeholders(text)
    }

    pub(super) fn with_fragments(&self, text: &str) -> String {
        let mut out = text.to_string();
        for evidence in self.fragments_in(text) {
            out.push(' ');
            out.push_str(&evidence);
        }
        out
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one record per executed command, every field of it"
    )]
    pub(super) fn record(
        &mut self,
        fields: &[Field<'_>],
        layers: Vec<Layer>,
        call: &Call,
        state: &State<'_>,
        cwd: Option<&Cwd>,
        reads_code_from_stdin: bool,
        code_substitutions: Vec<Range<usize>>,
    ) -> usize {
        let index = self.model.invocations.len();
        if index >= MAX_INVOCATIONS {
            self.exhausted = true;
            return index;
        }
        let feed = call
            .feed
            .clone()
            .unwrap_or_else(|| self.stdin.last().cloned().unwrap_or(Feed::Kernel));
        let stdin = match feed {
            Feed::Code(text) => Input::Text(text),
            Feed::File(path) => Input::File(path),
            Feed::Pipe | Feed::Hidden(_) | Feed::Piped(_) => Input::Pipe,
            Feed::Kernel => Input::Inherited,
        };
        let stdout = call
            .output
            .clone()
            .unwrap_or_else(|| self.stdout.last().cloned().unwrap_or(Output::Transcript));
        let argv: Vec<Arg> = fields.iter().map(|field| field.arg.clone()).collect();
        let name = argv
            .first()
            .and_then(Arg::known)
            .map(|text| text.rsplit('/').next().unwrap_or(text).to_lowercase());
        for range in call.all_subs() {
            self.model.captures.push(Capture {
                range,
                consumer: index,
            });
        }
        self.model.invocations.push(Invocation {
            argv,
            layers,
            env: call.env.clone(),
            git_env: state.vars.exported_changes("GIT_"),
            name,
            cwd: cwd.cloned().unwrap_or_else(|| state.cwd.clone()),
            stage: self.stage,
            depth_in_stage: self.stage_depth,
            stdin,
            stdout,
            context: self.via.clone(),
            reads_code_from_stdin,
            code_substitutions,
            index,
        });
        index
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Share {
    /// The code runs in the current shell (`eval`, `source`, a function).
    Shared,
    /// The code runs in a child shell.
    Copy,
}

/// `> target`: devices that are the transcript count as it.
fn file_output(target: Arg) -> Output {
    match target.known() {
        Some(
            "/dev/stdout" | "/dev/stderr" | "/dev/tty" | "/dev/fd/1" | "/dev/fd/2"
            | "/proc/self/fd/1" | "/proc/self/fd/2",
        ) => Output::Transcript,
        Some(_) | None => Output::File(target),
    }
}

pub(super) fn shown_fields(fields: &[Field<'_>]) -> String {
    fields
        .iter()
        .map(|field| match (&field.arg, field.word) {
            (Arg::Unknown(_), Some(word)) => word.raw.clone(),
            (arg, _) => arg.shown(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// After a compound command whose parts may or may not have run: what
/// differs from `before` is no longer certain.
pub(super) fn merge(state: &mut State<'_>, before: &State<'_>) {
    state.vars.merge(&before.vars);
    let mut cwd = state.cwd.clone();
    cwd.union(&before.cwd);
    state.cwd = cwd;
}

/// `cd x || exit 1`, `cd x || { echo; exit 1; }`: later commands only run
/// when the `cd` worked.
fn exits_on_failure(item: &AndOr) -> bool {
    let Some((Connector::Or, pipeline)) = item.rest.last() else {
        return false;
    };
    let is_exit = |command: &Command| -> bool {
        let simple_exit = |command: &Command| match command {
            Command::Simple(simple) => simple
                .words
                .first()
                .and_then(Word::as_static)
                .is_some_and(|word| matches!(word.as_str(), "exit" | "return")),
            _ => false,
        };
        match command {
            Command::Group(list, _) => list
                .items
                .iter()
                .any(|item| item.first.commands.iter().any(simple_exit)),
            other => simple_exit(other),
        }
    };
    pipeline.commands.iter().any(is_exit)
}

/// What a substitution prints when the text fixes it (`$(pwd)`).
fn command_output(body: &List, state: &State<'_>) -> Option<String> {
    let [item] = body.items.as_slice() else {
        return None;
    };
    let [Command::Simple(command)] = item.first.commands.as_slice() else {
        return None;
    };
    if !item.rest.is_empty() || !command.redirects.is_empty() {
        return None;
    }
    let words: Vec<String> = command
        .words
        .iter()
        .map(Word::as_static)
        .collect::<Option<_>>()?;
    // What the substitution yields after the shell strips trailing newlines.
    match words
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["pwd"] => match (state.cwd.dirs.as_slice(), &state.cwd.unknown) {
            ([dir], None) => Some(dir.display().to_string()),
            _ => None,
        },
        ["echo", rest @ ..] if rest.first().is_none_or(|first| !first.starts_with('-')) => {
            Some(rest.join(" "))
        }
        ["printf", format] if !format.contains(['%', '\\']) => Some((*format).to_string()),
        ["printf", "%s", rest @ ..] => Some(rest.concat()),
        _ => None,
    }
}

/// The text a fixed-output command prints (`echo x`, `printf 'x\n'`,
/// `cat file`), when the text decides it.
pub(super) fn emitted_text(
    invocation: &Invocation,
    written: &HashMap<PathBuf, String>,
) -> Option<String> {
    let program = invocation.program()?;
    let args: Vec<&str> = invocation.argv[1..]
        .iter()
        .map(Arg::known)
        .collect::<Option<_>>()?;
    match program {
        "echo" => {
            let args: Vec<&str> = args
                .into_iter()
                .skip_while(|arg| matches!(*arg, "-n" | "-e" | "-E"))
                .collect();
            Some(args.join(" ") + "\n")
        }
        "printf" => {
            let (format, rest) = args.split_first()?;
            if rest.is_empty() && !format.contains('%') {
                return Some(format.replace("\\n", "\n").replace("\\t", "\t"));
            }
            if matches!(*format, "%s\\n" | "%s\n") {
                return Some(rest.iter().fold(String::new(), |mut out, arg| {
                    out.push_str(arg);
                    out.push('\n');
                    out
                }));
            }
            if matches!(*format, "%s") {
                return Some(rest.concat());
            }
            None
        }
        "cat" => match (&invocation.stdin, args.as_slice()) {
            (Input::Text(text), [] | ["-"]) => Some(text.clone()),
            (_, [path]) => {
                let dir = invocation.cwd.dirs.first()?;
                let full = super::files::join(dir, path);
                written
                    .get(&full)
                    .cloned()
                    .or_else(|| super::files::read_script(&full))
            }
            _ => None,
        },
        _ => None,
    }
}
