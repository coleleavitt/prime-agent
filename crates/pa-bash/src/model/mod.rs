//! The command model the rules judge: every command the script would run,
//! after quoting, variables the text fixes, wrappers (`env`, `timeout`,
//! `sudo`, `xargs`, ...) and nested code (`sh -c`, `eval`, `ssh host CMD`,
//! here-documents fed to a shell, script files in the workspace) are
//! resolved; and an explicit [`Opaque`] node for each place where code runs
//! that the text does not show (a script outside the workspace, a shell
//! reading a pipe, `eval "$x"`, a command word only known at run time).
//!
//! A rule refuses an opaque node only when the node's own visible text
//! carries the rule's evidence ([`evidence`]): running code the guard cannot
//! read is ordinary work, and the OS sandbox contains it.

mod commands;
pub(crate) mod evidence;
pub(crate) mod files;
pub(crate) mod value;
mod walker;
pub(crate) mod wrappers;

use std::ops::Range;
use std::path::PathBuf;

use crate::context::GuardContext;
use crate::script::Script;

pub(crate) use value::Arg;

/// What one script runs.
#[derive(Debug, Clone, Default)]
pub(crate) struct Model {
    pub invocations: Vec<Invocation>,
    pub opaques: Vec<Opaque>,
    pub stages: Vec<Stage>,
    /// Command substitutions and the command whose words (or here-document)
    /// they expand into.
    pub captures: Vec<Capture>,
}

/// The commands of a substitution (a range of [`Model::invocations`]) and
/// the command their output becomes part of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Capture {
    pub range: Range<usize>,
    pub consumer: usize,
}

impl Model {
    /// Build the model of `script` in `context`.
    pub(crate) fn build(script: &Script<'_>, context: &GuardContext) -> Self {
        walker::build(script.script, context)
    }

    /// The commands that write into `invocation`'s stdin through the
    /// pipelines around it (every earlier stage, outward).
    pub(crate) fn producers(&self, invocation: &Invocation) -> Vec<&Invocation> {
        let mut out = Vec::new();
        let mut stage = invocation.stage;
        while let Some(index) = stage {
            let current = &self.stages[index];
            for earlier in &self.stages {
                if earlier.pipeline == current.pipeline && earlier.index < current.index {
                    out.extend(&self.invocations[earlier.range.clone()]);
                }
            }
            stage = current.parent;
        }
        out
    }

    /// The command a captured `invocation`'s output expands into.
    pub(crate) fn capturer(&self, invocation: &Invocation) -> Option<&Invocation> {
        self.captures
            .iter()
            .filter(|capture| capture.range.contains(&invocation.index))
            .min_by_key(|capture| capture.range.len())
            .and_then(|capture| self.invocations.get(capture.consumer))
    }

    /// The commands reading `invocation`'s stdout through its pipeline (the
    /// next stage).
    pub(crate) fn consumers(&self, invocation: &Invocation) -> Vec<&Invocation> {
        let Some(index) = invocation.stage else {
            return Vec::new();
        };
        let current = &self.stages[index];
        self.stages
            .iter()
            .filter(|stage| stage.pipeline == current.pipeline && stage.index == current.index + 1)
            .flat_map(|stage| &self.invocations[stage.range.clone()])
            .collect()
    }

    /// The first command of the stage after `invocation`'s, when its stdout
    /// is piped.
    pub(crate) fn next_stage_head(&self, invocation: &Invocation) -> Option<&Invocation> {
        let index = invocation.stage?;
        let current = &self.stages[index];
        let next = self
            .stages
            .iter()
            .find(|stage| stage.pipeline == current.pipeline && stage.index == current.index + 1)?;
        self.invocations[next.range.clone()]
            .iter()
            .find(|candidate| candidate.depth_in_stage == 0)
            .or_else(|| self.invocations.get(next.range.start))
    }
}

/// One stage of a pipeline: the invocations it runs (a range of
/// [`Model::invocations`], nested stages included).
#[derive(Debug, Clone)]
pub(crate) struct Stage {
    pub pipeline: usize,
    pub index: usize,
    pub range: Range<usize>,
    /// The stage this pipeline sits inside, when it is nested in one.
    pub parent: Option<usize>,
}

/// One command the script would run.
#[derive(Debug, Clone)]
pub(crate) struct Invocation {
    /// The argv after every wrapper was taken off: `argv[0]` is the program.
    pub argv: Vec<Arg>,
    /// The wrappers in front of it, outermost first (`sudo`, `env`, ...).
    pub layers: Vec<Layer>,
    /// Assignments the command runs with (`FOO=1 cmd`, `env FOO=1 cmd`).
    pub env: Vec<(String, Arg)>,
    /// `GIT_*` variables the script set or unset before it (`None`: unset),
    /// which a git probe must replay.
    pub git_env: Vec<(String, Option<Arg>)>,
    /// The program's name: `argv[0]`'s basename, lowercased (macOS
    /// resolves `GIT` and `Sudo` case-insensitively).
    pub name: Option<String>,
    pub cwd: Cwd,
    /// Its innermost pipeline stage ([`Model::stages`]).
    pub stage: Option<usize>,
    /// How deep inside its stage it sits (0: the stage's own command).
    pub depth_in_stage: usize,
    pub stdin: Input,
    pub stdout: Output,
    /// The nested code it sits in, outermost first.
    pub context: Vec<Via>,
    /// For a shell, `eval` or `source`: it reads the code it runs from its
    /// stdin.
    pub reads_code_from_stdin: bool,
    /// For a shell, `eval` or `source`: the commands of the substitutions in
    /// the words its code comes from (`sh -c "$(curl ...)"`, `bash
    /// <(curl ...)`, a here-document body's `$(...)`).
    pub code_substitutions: Vec<Range<usize>>,
    /// Its own position in [`Model::invocations`].
    pub index: usize,
}

impl Invocation {
    /// The program name (`argv[0]`'s basename, lowercased) when it is
    /// fixed.
    pub(crate) fn program(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Whether the program is `name`: its fixed name, or a command-word
    /// pattern (`chmo[d]`, `[s]udo`) that can only expand to it.
    pub(crate) fn runs(&self, name: &str) -> bool {
        match self.argv.first() {
            Some(Arg::Known(_)) => self.program() == Some(name),
            Some(Arg::Pattern(pattern)) => {
                let base = pattern.rsplit('/').next().unwrap_or(pattern);
                base.chars().any(|ch| ch.is_ascii_alphabetic())
                    && value::pattern_matches(&base.to_lowercase(), name)
            }
            Some(Arg::Unknown(_)) | None => false,
        }
    }

    /// The command as the rule shows it.
    pub(crate) fn shown(&self) -> String {
        let quoted = |arg: &Arg| {
            let text = arg.shown();
            let text = hide_placeholders(&text);
            if text.is_empty() || text.contains(char::is_whitespace) {
                format!("'{text}'")
            } else {
                text
            }
        };
        let mut words: Vec<String> = Vec::new();
        for layer in &self.layers {
            words.push(layer.name.clone());
            words.extend(layer.args.iter().map(quoted));
        }
        words.extend(self.argv.iter().map(quoted));
        shorten(&words.join(" "), 160)
    }

    /// Where the command sits, for a message (`""` at the top level).
    pub(crate) fn place(&self) -> String {
        describe_context(&self.context)
    }
}

/// `text` with the model's placeholders for run-time values shown as `...`.
pub(crate) fn hide_placeholders(text: &str) -> String {
    let mut out = text.to_string();
    while let Some(at) = out.find("${__PA_FRAGMENT_") {
        let end = out[at..].find("__}").map_or(out.len(), |end| at + end + 3);
        out.replace_range(at..end, "...");
    }
    out
}

/// `text` cut to `limit` chars with an ellipsis.
pub(crate) fn shorten(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}...")
}

/// The nesting a command or opaque node sits in, as message text.
pub(crate) fn describe_context(context: &[Via]) -> String {
    let mut out = String::new();
    for via in context.iter().rev() {
        let piece = match via {
            Via::Payload { runner } => format!("inside the `{runner}` payload"),
            Via::Eval => "inside an `eval` payload".to_string(),
            Via::Script { path } => format!("in the script {path}"),
            Via::Stdin { runner } => format!("in the code `{runner}` reads from its stdin"),
            Via::Remote { host } => format!("on the remote host {host}"),
            Via::Function { name } => format!("in the function {name}"),
            Via::Alias { name } => format!("in the alias {name}"),
            Via::Trap => "in a trap handler".to_string(),
            Via::Substitution => continue,
        };
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&piece);
    }
    out
}

/// A wrapper in front of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layer {
    /// The wrapper's program name (basename).
    pub name: String,
    /// Its own options and operands.
    pub args: Vec<Arg>,
}

/// The directories a command may run in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cwd {
    /// Every directory it may run in (one, unless an earlier `cd` may have
    /// failed).
    pub dirs: Vec<PathBuf>,
    /// The relocation that made the directory unknown (`cd "$dir"`).
    pub unknown: Option<String>,
}

impl Cwd {
    pub(crate) fn at(dir: PathBuf) -> Self {
        Self {
            dirs: vec![dir],
            unknown: None,
        }
    }

    pub(crate) fn union(&mut self, other: &Cwd) {
        for dir in &other.dirs {
            if !self.dirs.contains(dir) {
                self.dirs.push(dir.clone());
            }
        }
        if self.unknown.is_none() {
            self.unknown.clone_from(&other.unknown);
        }
    }
}

/// Where a command's stdin comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Input {
    /// The kernel's (nothing the script controls).
    Inherited,
    /// An earlier pipeline stage.
    Pipe,
    /// `< file`.
    File(Arg),
    /// A here-document or here-string: its text, with unknown parts.
    Text(String),
}

/// Where a command's stdout goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Output {
    /// The transcript (the kernel reads it back to the model).
    Transcript,
    /// The next pipeline stage.
    Pipe,
    /// `> file` (`/dev/null` included).
    File(Arg),
    /// A command substitution captures it.
    Captured,
}

/// Nested code a command sits in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Via {
    /// A `-c` string (`sh -c`, `bash -lc`, `su -c`, `watch`, `flock -c`).
    Payload {
        runner: String,
    },
    Eval,
    Script {
        path: String,
    },
    /// Code a shell reads from its stdin (a here-document, a pipe).
    Stdin {
        runner: String,
    },
    Remote {
        host: String,
    },
    Function {
        name: String,
    },
    Alias {
        name: String,
    },
    Trap,
    Substitution,
}

/// Code that runs where the guard cannot read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Opaque {
    pub kind: OpaqueKind,
    /// What the message names (`bash /tmp/x.sh`, `eval "$cmd"`).
    pub shown: String,
    /// The visible text the hidden code comes from: the only text a rule
    /// may read evidence from.
    pub evidence: String,
    pub context: Vec<Via>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpaqueKind {
    /// A script file the guard does not read (outside the workspace, or
    /// missing).
    Script,
    /// A shell reading code from a file or a stream it cannot see.
    Stdin,
    /// A shell reading the output of earlier pipeline stages the model
    /// cannot reconstruct (the stages themselves are in the model).
    Pipe,
    /// Code assembled from a value only known at run time (`eval "$x"`,
    /// `sh -c "$(cat cmd)"`).
    Dynamic,
    /// A command word only known at run time (`$CMD args`).
    CommandWord,
    /// Text the parser could not read, or nesting past the bound.
    Unparsed,
}

impl Opaque {
    /// The message phrase for the hidden code.
    pub(crate) fn describe(&self) -> String {
        let shown = shorten(&self.shown, 120);
        let what = match self.kind {
            OpaqueKind::Script => format!("the script run by `{shown}` is not one the guard reads"),
            OpaqueKind::Stdin | OpaqueKind::Pipe => {
                format!("`{shown}` runs code from its stdin that the guard cannot see")
            }
            OpaqueKind::Dynamic => format!("`{shown}` runs code assembled at run time"),
            OpaqueKind::CommandWord => {
                format!("the command word of `{shown}` is only known at run time")
            }
            OpaqueKind::Unparsed => format!("the guard cannot read `{shown}`"),
        };
        let place = describe_context(&self.context);
        if place.is_empty() {
            what
        } else {
            format!("{what} ({place})")
        }
    }
}

#[cfg(test)]
mod tests;
