//! What builtins change in the walk's state: the directory (`cd`), variables
//! (`export`, `unset`, `read`, `set`), aliases, hash entries, shell options
//! and traps.

use super::commands::Pending;
use super::files::join;
use super::value::{Arg, Lookup, Unknown, Var};
use super::walker::{Feed, Field, Share, State, Walker};
use super::{Cwd, Via, wrappers};

/// A builtin whose effect on the walk's state the model applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Builtin {
    /// `cd`, `pushd`, `popd`.
    Directory,
    /// `export`, `declare`, `typeset`, `local`, `readonly`.
    Declare,
    Unset,
    Shopt,
    Unalias,
    /// `read`, `mapfile`, `readarray`, `getopts`.
    Read,
    Set,
    Hash,
    Alias,
    Trap,
}

impl Builtin {
    /// The builtin a lowercased command word names.
    pub(super) fn of(name: &str) -> Option<Self> {
        Some(match name {
            "cd" | "pushd" | "popd" => Self::Directory,
            "export" | "declare" | "typeset" | "local" | "readonly" => Self::Declare,
            "unset" => Self::Unset,
            "shopt" => Self::Shopt,
            "unalias" => Self::Unalias,
            "read" | "mapfile" | "readarray" | "getopts" => Self::Read,
            "set" => Self::Set,
            "hash" => Self::Hash,
            "alias" => Self::Alias,
            "trap" => Self::Trap,
            _ => return None,
        })
    }
}

impl Walker {
    /// Apply what `builtin` (run as `name`) changes; the command is already
    /// recorded.
    pub(super) fn apply_builtin(
        &mut self,
        builtin: Builtin,
        name: &str,
        command: &Pending<'_>,
        state: &mut State<'_>,
    ) {
        let fields = &command.fields;
        match builtin {
            Builtin::Directory => change_directory(name, fields, &command.call.env, state),
            Builtin::Declare => {
                let flags = || {
                    fields[1..]
                        .iter()
                        .filter_map(|field| field.arg.known())
                        .filter(|text| text.starts_with('-'))
                };
                let export = match name {
                    "export" => !flags().any(|flag| flag.contains('n')),
                    "declare" | "typeset" => flags().any(|flag| flag.contains('x')),
                    _ => false,
                };
                for field in &fields[1..] {
                    declare(field, state, export);
                }
            }
            Builtin::Unset => {
                for field in &fields[1..] {
                    if let Some(variable) = field.arg.known().filter(|text| !text.starts_with('-'))
                    {
                        state.vars.unset(variable);
                        state.functions.remove(variable);
                    }
                }
            }
            Builtin::Shopt => {
                let words: Vec<&str> = fields[1..]
                    .iter()
                    .filter_map(|field| field.arg.known())
                    .collect();
                if words.contains(&"expand_aliases") {
                    if words.contains(&"-s") {
                        state.expand_aliases = true;
                    } else if words.contains(&"-u") {
                        state.expand_aliases = false;
                    }
                }
            }
            Builtin::Unalias => unalias(fields, state),
            Builtin::Read => self.read(name, command, state),
            Builtin::Set => self.set(fields, state),
            Builtin::Hash => {
                if fields[1..].iter().any(|field| {
                    field
                        .arg
                        .known()
                        .is_some_and(|flag| flag.starts_with('-') && flag.contains('r'))
                }) {
                    state.hashed.clear();
                }
                if let Some((path, names)) = hash_registration(fields) {
                    for registered in names {
                        state.hashed.insert(registered, path.clone());
                    }
                }
            }
            Builtin::Alias => {
                for field in &fields[1..] {
                    // A definition runs nothing: the body is judged where the
                    // alias is used.
                    if let Some((alias, body)) =
                        field.arg.known().and_then(|text| text.split_once('='))
                    {
                        state.aliases.insert(alias.to_string(), body.to_string());
                    }
                }
            }
            Builtin::Trap => self.trap(command, state),
        }
    }

    /// `read VAR...`, `mapfile`: the variables take values from the
    /// command's input.
    fn read(&self, name: &str, command: &Pending<'_>, state: &mut State<'_>) {
        let fields = &command.fields;
        let source = match &command.call.feed {
            Some(Feed::Code(text) | Feed::Hidden(text) | Feed::Piped(text)) => text.clone(),
            Some(Feed::File(path)) => path.evidence(),
            Some(Feed::Kernel | Feed::Pipe) | None => self.producer_text(),
        };
        let mut skip = false;
        for field in &fields[1..] {
            if skip {
                skip = false;
                continue;
            }
            match field.arg.known() {
                Some(flag) if flag.starts_with('-') => {
                    skip = matches!(
                        flag,
                        "-a" | "-d"
                            | "-n"
                            | "-N"
                            | "-p"
                            | "-t"
                            | "-u"
                            | "-i"
                            | "-C"
                            | "-c"
                            | "-O"
                            | "-s"
                    );
                }
                Some(variable) if wrappers::is_name(variable) => state.vars.assign(
                    variable,
                    Var {
                        value: None,
                        source: source.clone(),
                    },
                ),
                Some(_) | None => {}
            }
        }
        if fields.len() == 1 || name != "read" {
            state.vars.assign(
                if name == "read" { "REPLY" } else { "MAPFILE" },
                Var {
                    value: None,
                    source,
                },
            );
        }
    }

    /// `set`: `-e`, `allexport`, and the positional parameters after `--`.
    fn set(&mut self, fields: &[Field<'_>], state: &mut State<'_>) {
        let mut positional = None;
        for (at, field) in fields.iter().enumerate().skip(1) {
            match field.arg.known() {
                Some("--") => {
                    positional = Some(at + 1);
                    break;
                }
                Some("-o")
                    if fields.get(at + 1).and_then(|field| field.arg.known())
                        == Some("allexport") =>
                {
                    state.vars.set_allexport(true);
                }
                Some("+o")
                    if fields.get(at + 1).and_then(|field| field.arg.known())
                        == Some("allexport") =>
                {
                    state.vars.set_allexport(false);
                }
                Some(flag) if flag.starts_with('-') && !flag.starts_with("--") => {
                    if flag.contains('e') {
                        self.errexit = true;
                    }
                    if flag.contains('a') {
                        state.vars.set_allexport(true);
                    }
                }
                Some(flag) if flag.starts_with('+') && flag.contains('a') => {
                    state.vars.set_allexport(false);
                }
                Some(_) | None => {}
            }
        }
        if let Some(at) = positional {
            state.positional = fields[at..].iter().map(|field| field.arg.clone()).collect();
        }
    }

    /// `trap CODE SIGNAL...`: the handler's code.
    fn trap(&mut self, command: &Pending<'_>, state: &mut State<'_>) {
        let action = command.fields[1..]
            .iter()
            .find(|field| field.arg.known() != Some("--"));
        let Some(action) = action else {
            return;
        };
        if action.arg.known().is_none_or(|text| !text.starts_with('-')) {
            let code = self.field_code(action, state, &command.call.pieces);
            // The handler may run before any later command (a DEBUG or EXIT
            // trap): what it changes may hold.
            let mut child = state.clone();
            self.code(&code, Via::Trap, &mut child, Share::Shared, None, None);
            state.cwd.union(&child.cwd);
        }
    }
}

/// `unalias NAME...`, `unalias -a`.
fn unalias(fields: &[Field<'_>], state: &mut State<'_>) {
    // bash's `unalias` takes only `-a`; any other option is a usage error
    // that removes nothing.
    let mut options_done = false;
    let mut valid = true;
    for field in &fields[1..] {
        match field.arg.known() {
            Some("--") if !options_done => options_done = true,
            Some(flag) if !options_done && flag.starts_with('-') && flag.len() > 1 => {
                valid &= flag[1..].chars().all(|letter| letter == 'a');
            }
            Some(_) | None => options_done = true,
        }
    }
    if !valid {
        return;
    }
    for field in &fields[1..] {
        match field.arg.known() {
            Some(flag)
                if flag.len() > 1
                    && flag.starts_with('-')
                    && flag[1..].chars().all(|letter| letter == 'a') =>
            {
                state.aliases.clear();
            }
            Some(alias) if !alias.starts_with('-') => {
                state.aliases.remove(alias);
            }
            Some(_) | None => {}
        }
    }
}

fn change_directory(
    name: &str,
    fields: &[Field<'_>],
    env: &[(String, Arg)],
    state: &mut State<'_>,
) {
    if name == "popd" {
        state.cwd = Cwd {
            dirs: Vec::new(),
            unknown: Some("popd".to_string()),
        };
        return;
    }
    let target = fields[1..]
        .iter()
        .find(|field| {
            field
                .arg
                .known()
                .is_none_or(|text| !text.starts_with('-') || text == "-")
        })
        .map(|field| field.arg.clone());
    // `HOME=dir cd` goes to the HOME of its own prefix.
    let prefixed = env
        .iter()
        .rev()
        .find(|(name, _)| name == "HOME")
        .map(|(_, value)| value.clone());
    let target = target
        .or(prefixed)
        .unwrap_or_else(|| match state.vars.get("HOME") {
            Lookup::Value(home) => Arg::Known(home),
            Lookup::Unknown(source) => Arg::Unknown(Unknown {
                prefix: String::new(),
                written: "cd ($HOME)".to_string(),
                source,
                input: false,
            }),
            Lookup::Unset => Arg::Known(String::new()),
        });
    let next = relocate(&state.cwd, &target, state);
    state.cwd = next;
}

/// The directory `target` names from `base` (`cd` semantics, CDPATH
/// included).
pub(super) fn relocate(base: &Cwd, target: &Arg, state: &State<'_>) -> Cwd {
    let unknown = |why: String| Cwd {
        dirs: Vec::new(),
        unknown: Some(why),
    };
    let Arg::Known(text) = target else {
        return unknown(format!("cd {}", target.shown()));
    };
    if text == "-" {
        return unknown("cd -".to_string());
    }
    if text.is_empty() {
        return base.clone();
    }
    let cdpath = match state.vars.get("CDPATH") {
        Lookup::Value(value) => Some(value),
        Lookup::Unknown(_) if !text.starts_with('/') && !text.starts_with('.') => {
            return unknown(format!("cd {text} (CDPATH is set)"));
        }
        Lookup::Unknown(_) | Lookup::Unset => None,
    };
    let mut dirs = Vec::new();
    for dir in &base.dirs {
        let mut found = None;
        if let Some(cdpath) = cdpath.as_deref().filter(|cdpath| !cdpath.is_empty()) {
            let plain = !text.starts_with('/')
                && !text.starts_with("./")
                && !text.starts_with("../")
                && text != "."
                && text != "..";
            if plain {
                for entry in cdpath.split(':') {
                    let root = if entry.is_empty() {
                        dir.clone()
                    } else {
                        join(dir, entry)
                    };
                    let candidate = join(&root, text);
                    if candidate.is_dir() {
                        found = Some(candidate);
                        break;
                    }
                }
            }
        }
        let next = found.unwrap_or_else(|| join(dir, text));
        if !dirs.contains(&next) {
            dirs.push(next);
        }
    }
    Cwd {
        dirs,
        unknown: base.unknown.clone(),
    }
}

/// `export NAME=value` and friends; `export` marks the name exported.
fn declare(field: &Field<'_>, state: &mut State<'_>, export: bool) {
    let text = match &field.arg {
        Arg::Known(text) => text.clone(),
        Arg::Pattern(_) => return,
        Arg::Unknown(unknown) => {
            if let Some((name, _)) = unknown.prefix.split_once('=') {
                let name = name.strip_suffix('+').unwrap_or(name);
                if wrappers::is_name(name) {
                    state.vars.assign(
                        name,
                        Var {
                            value: None,
                            source: format!("{} {}", unknown.written, unknown.source),
                        },
                    );
                    if export {
                        state.vars.export(name);
                    }
                }
            }
            return;
        }
    };
    if let Some((name, value)) = text.split_once('=') {
        let (name, append) = match name.strip_suffix('+') {
            Some(name) => (name, true),
            None => (name, false),
        };
        if wrappers::is_name(name) {
            state.vars.assign(
                name,
                Var {
                    value: (!append).then(|| value.to_string()),
                    source: value.to_string(),
                },
            );
            if export {
                state.vars.export(name);
            }
        }
    } else if export && wrappers::is_name(&text) {
        state.vars.export(&text);
    }
}

/// `hash -p PATH NAME...`.
fn hash_registration(fields: &[Field<'_>]) -> Option<(Arg, Vec<String>)> {
    let mut args = fields[1..].iter().map(|field| &field.arg);
    let mut path = None;
    while let Some(arg) = args.next() {
        let text = arg.known()?;
        if let Some(flags) = text.strip_prefix('-') {
            if let Some(at) = flags.find('p') {
                let glued = &flags[at + 1..];
                path = if glued.is_empty() {
                    args.next().cloned()
                } else {
                    Some(Arg::Known(glued.to_string()))
                };
                break;
            }
            continue;
        }
        return None;
    }
    let names = args
        .filter_map(|arg| arg.known().map(str::to_string))
        .collect();
    Some((path?, names))
}
